//! The `RUN` child between `clone` and `execve`.
//!
//! The child is cloned from a multi-threaded process: another thread may have
//! held the allocator's lock at that instant, and the C library's thread data
//! still describes the spawner thread. So the child never allocates, never
//! takes a lock, never formats or panics on purpose, and only makes system
//! calls: rustix's raw-syscall backend with `&CStr` arguments (which it
//! passes through without copying), and `libc` for the few calls rustix
//! lacks. Every string and table was built before the clone ([`ChildArgs`]).
//!
//! A failing step writes `(step, errno)` to the error pipe and exits with
//! 127; the pipe is close-on-exec, so the parent reads end-of-file once
//! `execve` succeeds.

use std::ffi::{CStr, CString, c_char};
use std::marker::PhantomData;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};

use rustix::fs::{CWD, Mode, OFlags, ResolveFlags};
use rustix::io::Errno;
use rustix::mount::{MountFlags, MountPropagationFlags, UnmountFlags};
use rustix::process::{Gid, Signal, Uid};
use rustix::thread::{CapabilitySet, CapabilitySets};

use crate::error::Step;
use crate::plan::{DEV_NODES, DEV_SYMLINKS, DEV_TMPFS, DEVPTS, SHM};

/// How the child enters the root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootEntry {
    /// `pivot_root(".", ".")`, then detach the old root.
    PivotRoot,
    /// `mount(".", "/", MS_MOVE)` then `chroot(".")`, as runc's
    /// `--no-pivot`, where seccomp blocks `pivot_root`.
    MoveMount,
}

/// One mount, as the child performs it. Targets are paths inside the root;
/// sources are host paths owned by ranikust.
#[derive(Debug)]
pub(crate) enum ChildMount {
    Proc {
        target: CString,
    },
    Sysfs {
        target: CString,
    },
    Dev {
        target: CString,
    },
    File {
        source: CString,
        target: CString,
    },
    Bind {
        source: CString,
        target: CString,
        /// Flags of the read-only remount, when read-only.
        remount: Option<MountFlags>,
    },
}

/// Where a standard stream goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChildStdio {
    Inherit,
    Null,
    /// A descriptor numbered 3 or higher, so `dup2` never overwrites another
    /// source.
    Fd(RawFd),
}

/// Everything the child needs, built before the clone.
#[derive(Debug)]
pub(crate) struct ChildArgs {
    pub(crate) root_dir: CString,
    pub(crate) mounts: Vec<ChildMount>,
    pub(crate) root_entry: RootEntry,
    pub(crate) hostname: Vec<u8>,
    pub(crate) loopback: bool,
    /// Capabilities to drop from the bounding set, and how many.
    pub(crate) drops: ([u32; 64], usize),
    /// The capabilities kept (Docker's defaults), as a mask.
    pub(crate) keep: u64,
    pub(crate) uid: Uid,
    pub(crate) gid: Gid,
    pub(crate) groups: Vec<Gid>,
    pub(crate) workdir: CString,
    pub(crate) stdio: [ChildStdio; 3],
    pub(crate) argv: Vec<CString>,
    pub(crate) envp: Vec<CString>,
    pub(crate) candidates: Vec<CString>,
}

/// The `NULL`-terminated pointer arrays `execve` takes, pointing into a
/// [`ChildArgs`] that outlives them.
pub(crate) struct ExecPtrs<'a> {
    argv: Vec<*const c_char>,
    envp: Vec<*const c_char>,
    _args: PhantomData<&'a ChildArgs>,
}

impl<'a> ExecPtrs<'a> {
    pub(crate) fn new(args: &'a ChildArgs) -> Self {
        let ptrs = |v: &[CString]| {
            v.iter()
                .map(|s| s.as_ptr())
                .chain(std::iter::once(std::ptr::null()))
                .collect()
        };
        Self {
            argv: ptrs(&args.argv),
            envp: ptrs(&args.envp),
            _args: PhantomData,
        }
    }
}

/// Raw descriptors of the child, valid in the child after the clone.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ChildFds {
    /// Write end of the error pipe (close-on-exec).
    pub(crate) err_w: RawFd,
    /// Read end of the spawner's alive pipe (close-on-exec, non-blocking).
    pub(crate) alive_r: RawFd,
    /// The child's copy of the write end, closed first.
    pub(crate) alive_w: RawFd,
}

/// `/proc/self/fd/<fd>`, NUL-terminated, on the stack: mounting onto it acts
/// on exactly the inode the descriptor holds, as runc does.
pub(crate) struct FdPath {
    buf: [u8; 32],
    len: usize,
}

impl FdPath {
    const PREFIX: &'static [u8] = b"/proc/self/fd/";

    pub(crate) fn new(fd: RawFd) -> Self {
        let mut buf = [0u8; 32];
        let mut len = 0;
        for &b in Self::PREFIX {
            buf[len] = b;
            len += 1;
        }
        let mut digits = [0u8; 10];
        let mut n = fd.unsigned_abs();
        let mut count = 0;
        loop {
            digits[count] = b'0' + (n % 10).to_le_bytes()[0];
            count += 1;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        while count > 0 {
            count -= 1;
            buf[len] = digits[count];
            len += 1;
        }
        // The terminating NUL is already there.
        len += 1;
        Self { buf, len }
    }

    pub(crate) fn as_cstr(&self) -> Option<&CStr> {
        CStr::from_bytes_with_nul(self.buf.get(..self.len)?).ok()
    }

    /// The path; empty (which every syscall refuses) if it were malformed,
    /// which [`fd_path`] rules out.
    fn path(&self) -> &CStr {
        self.as_cstr().unwrap_or(c"")
    }
}

/// The child: performs the sequence of `design/sandbox.md` and executes the
/// command, or reports the failing step and exits.
///
/// # Safety
///
/// Call only in the child of a `clone` without `CLONE_VM`, right after it
/// returned 0, with `args`, `ptrs` and `fds` prepared by the parent before
/// the clone.
pub(crate) unsafe fn run(args: &ChildArgs, ptrs: &ExecPtrs<'_>, fds: ChildFds) -> ! {
    let err = fds.err_w;
    // SAFETY: forwarded from this function's contract.
    unsafe { arm_death_signal(fds) };

    // Nothing mounted from here on propagates to the build's namespace.
    check(
        err,
        Step::PrivateMounts,
        rustix::mount::mount_change(
            c"/",
            MountPropagationFlags::PRIVATE | MountPropagationFlags::REC,
        ),
    );
    let root_dir = args.root_dir.as_c_str();
    check(
        err,
        Step::BindRoot,
        rustix::mount::mount_bind_recursive(root_dir, root_dir),
    );
    check(err, Step::EnterRoot, rustix::process::chdir(root_dir));
    // Opened after the bind, so it is the bind's root: targets resolved from
    // it are inside the mount `pivot_root` enters.
    let root = check(
        err,
        Step::EnterRoot,
        rustix::fs::openat(
            CWD,
            c".",
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        ),
    );
    for m in &args.mounts {
        mount_one(err, root.as_fd(), m);
    }

    check(err, Step::EnterRoot, rustix::process::fchdir(&root));
    match args.root_entry {
        RootEntry::PivotRoot => {
            check(
                err,
                Step::PivotRoot,
                rustix::process::pivot_root(c".", c"."),
            );
            // The old root is stacked on "." now; detach it. The mounts are
            // private, so the unmount does not propagate.
            check(
                err,
                Step::DetachOldRoot,
                rustix::mount::unmount(c".", UnmountFlags::DETACH),
            );
        }
        RootEntry::MoveMount => {
            // The old root stays mounted underneath; root keeps
            // CAP_SYS_CHROOT for now [open: Q-077].
            check(err, Step::MoveRoot, rustix::mount::mount_move(c".", c"/"));
            check(err, Step::ChrootAfterMove, rustix::process::chroot(c"."));
        }
    }
    check(err, Step::EnterRoot, rustix::process::chdir(c"/"));
    drop(root);

    // A seccomp profile may refuse it; the host name is cosmetic.
    let _ = rustix::system::sethostname(&args.hostname);
    if args.loopback {
        loopback_up(err);
    }

    drop_capabilities(err, args);
    switch_user_and_exec(err, args, ptrs)
}

/// Arms `PR_SET_PDEATHSIG(SIGKILL)` and checks that the spawner did not die
/// before it was armed.
///
/// # Safety
///
/// As [`run`]: in the child, with descriptors prepared before the clone.
pub(crate) unsafe fn arm_death_signal(fds: ChildFds) {
    let err = fds.err_w;
    // SAFETY: `alive_w` is the child's own copy of the write end; it is
    // closed once here and never used again.
    unsafe { rustix::io::close(fds.alive_w) };
    check(
        err,
        Step::DeathSignal,
        rustix::process::set_parent_process_death_signal(Some(Signal::KILL)),
    );
    // `getppid` is 0 in a new PID namespace; the alive pipe tells instead.
    // SAFETY: `alive_r` is open in the child until `execve` closes it.
    let alive = unsafe { BorrowedFd::borrow_raw(fds.alive_r) };
    match rustix::io::read(alive, &mut [0u8; 1]) {
        Err(Errno::AGAIN) | Ok(1..) => {}
        Ok(0) => fail(err, Step::ParentGone, Errno::SRCH),
        Err(e) => fail(err, Step::ParentGone, e),
    }
}

/// `WORKDIR`, the `USER` ids, stdio, process state, then `execve`.
pub(crate) fn switch_user_and_exec(err: RawFd, args: &ChildArgs, ptrs: &ExecPtrs<'_>) -> ! {
    // Change to WORKDIR before switching user, in case only root may
    // traverse its parents, and again after if that was refused (runc).
    let workdir = args.workdir.as_c_str();
    let chdir_again = match rustix::process::chdir(workdir) {
        Ok(()) => false,
        Err(Errno::ACCESS) => true,
        Err(e) => fail(err, Step::Workdir, e),
    };
    check(
        err,
        Step::Groups,
        rustix::thread::set_thread_groups(&args.groups),
    );
    check(
        err,
        Step::Gid,
        rustix::thread::set_thread_res_gid(args.gid, args.gid, args.gid),
    );
    check(
        err,
        Step::Uid,
        rustix::thread::set_thread_res_uid(args.uid, args.uid, args.uid),
    );
    if chdir_again {
        check(err, Step::Workdir, rustix::process::chdir(workdir));
    }

    set_stdio(err, &args.stdio);
    reset_process_state();
    exec(err, args, ptrs)
}

fn mount_one(err: RawFd, root: BorrowedFd<'_>, m: &ChildMount) {
    match m {
        ChildMount::Proc { target } => {
            let t = resolve(err, root, target);
            let flags = MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC;
            check(
                err,
                Step::MountProc,
                rustix::mount::mount(
                    c"proc",
                    fd_path(err, &t).path(),
                    c"proc",
                    flags,
                    None::<&CStr>,
                ),
            );
        }
        ChildMount::Sysfs { target } => {
            let t = resolve(err, root, target);
            let flags =
                MountFlags::RDONLY | MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC;
            check(
                err,
                Step::MountSys,
                rustix::mount::mount(
                    c"sysfs",
                    fd_path(err, &t).path(),
                    c"sysfs",
                    flags,
                    None::<&CStr>,
                ),
            );
        }
        ChildMount::Dev { target } => {
            let t = resolve(err, root, target);
            let flags = MountFlags::NOSUID | MountFlags::STRICTATIME;
            check(
                err,
                Step::MountDev,
                rustix::mount::mount(
                    c"tmpfs",
                    fd_path(err, &t).path(),
                    c"tmpfs",
                    flags,
                    DEV_TMPFS,
                ),
            );
            drop(t);
            // Resolved again, it is the tmpfs, not the directory under it.
            let dev = resolve(err, root, target);
            populate_dev(err, dev.as_fd());
        }
        ChildMount::File { source, target } => {
            let t = resolve(err, root, target);
            check(
                err,
                Step::BindFile,
                rustix::mount::mount_bind(source.as_c_str(), fd_path(err, &t).path()),
            );
        }
        ChildMount::Bind {
            source,
            target,
            remount,
        } => {
            let t = resolve(err, root, target);
            check(
                err,
                Step::BindMount,
                rustix::mount::mount_bind(source.as_c_str(), fd_path(err, &t).path()),
            );
            drop(t);
            if let Some(flags) = *remount {
                let t = resolve(err, root, target);
                check(
                    err,
                    Step::Remount,
                    rustix::mount::mount_remount(fd_path(err, &t).path(), flags, c""),
                );
            }
        }
    }
}

/// A tmpfs `/dev`: the device nodes bound from the outer `/dev`, `pts`,
/// `shm` and the symlinks.
fn populate_dev(err: RawFd, dev: BorrowedFd<'_>) {
    for (name, source) in DEV_NODES {
        let node = check(
            err,
            Step::DevNode,
            rustix::fs::openat(
                dev,
                name,
                OFlags::CREATE | OFlags::EXCL | OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o666),
            ),
        );
        match rustix::mount::mount_bind(source, fd_path(err, &node).path()) {
            Ok(()) => {}
            // The outer environment lacks this node (no `/dev/tty` in some
            // sandboxes): leave it out rather than fail every `RUN`.
            Err(Errno::NOENT) => {
                drop(node);
                check(
                    err,
                    Step::DevNode,
                    rustix::fs::unlinkat(dev, name, rustix::fs::AtFlags::empty()),
                );
            }
            Err(e) => fail(err, Step::DevNode, e),
        }
    }
    for (name, fstype, flags, data) in [
        (
            c"pts",
            c"devpts",
            MountFlags::NOSUID | MountFlags::NOEXEC,
            DEVPTS,
        ),
        (
            c"shm",
            c"tmpfs",
            MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC,
            SHM,
        ),
    ] {
        check(
            err,
            Step::DevMounts,
            rustix::fs::mkdirat(dev, name, Mode::from_raw_mode(0o755)),
        );
        let dir = check(
            err,
            Step::DevMounts,
            rustix::fs::openat(
                dev,
                name,
                OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            ),
        );
        check(
            err,
            Step::DevMounts,
            rustix::mount::mount(fstype, fd_path(err, &dir).path(), fstype, flags, data),
        );
    }
    for (link, target) in DEV_SYMLINKS {
        check(
            err,
            Step::DevSymlink,
            rustix::fs::symlinkat(target, dev, link),
        );
    }
}

/// `target` inside the root, as an `O_PATH` descriptor: symlinks are followed
/// but cannot leave the root, magic links are refused.
fn resolve(err: RawFd, root: BorrowedFd<'_>, target: &CStr) -> OwnedFd {
    let mut attempts = 0;
    loop {
        match rustix::fs::openat2(
            root,
            target,
            OFlags::PATH | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::IN_ROOT | ResolveFlags::NO_MAGICLINKS,
        ) {
            Ok(fd) => return fd,
            // A rename raced the lookup; the kernel asks to retry.
            Err(Errno::AGAIN) if attempts < 16 => attempts += 1,
            Err(e) => fail(err, Step::ResolveTarget, e),
        }
    }
}

/// The path that names `fd`'s inode for `mount(2)`. The caller keeps `fd`
/// open while the path is used.
fn fd_path(err: RawFd, fd: &OwnedFd) -> FdPath {
    let p = FdPath::new(fd.as_raw_fd());
    if p.as_cstr().is_none() {
        fail(err, Step::ResolveTarget, Errno::INVAL);
    }
    p
}

/// Brings up `lo` in the new network namespace of `--network=none`, as
/// Docker does.
fn loopback_up(err: RawFd) {
    const SIOCGIFFLAGS: libc::c_ulong = 0x8913;
    const SIOCSIFFLAGS: libc::c_ulong = 0x8914;
    const IFF_UP: libc::c_short = 0x1;
    // SAFETY: a plain syscall; the returned descriptor is owned below.
    let raw = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if raw < 0 {
        fail(err, Step::Loopback, last_errno());
    }
    // SAFETY: `raw` is a socket this function just opened and owns.
    let sock = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY: `ifreq` is plain integers, unions and arrays; all-zero is a
    // valid value (empty name, zero flags).
    let mut ifr: libc::ifreq = unsafe { std::mem::zeroed() };
    ifr.ifr_name[0] = libc::c_char::from_ne_bytes(*b"l");
    ifr.ifr_name[1] = libc::c_char::from_ne_bytes(*b"o");
    // SAFETY: SIOCGIFFLAGS reads the name and writes `ifru_flags` of the
    // `ifreq` the pointer refers to, which lives across the call.
    let r = unsafe {
        libc::syscall(
            libc::SYS_ioctl,
            sock.as_raw_fd(),
            SIOCGIFFLAGS,
            &raw mut ifr,
        )
    };
    if r < 0 {
        fail(err, Step::Loopback, last_errno());
    }
    // SAFETY: SIOCGIFFLAGS filled the `ifru_flags` member of the union.
    unsafe { ifr.ifr_ifru.ifru_flags |= IFF_UP };
    // SAFETY: SIOCSIFFLAGS reads the `ifreq`, which lives across the call.
    let r = unsafe {
        libc::syscall(
            libc::SYS_ioctl,
            sock.as_raw_fd(),
            SIOCSIFFLAGS,
            &raw const ifr,
        )
    };
    if r < 0 {
        fail(err, Step::Loopback, last_errno());
    }
}

/// Bounding set to Docker's defaults, ambient set cleared, effective and
/// permitted set to the defaults (of those the process has), inheritable
/// empty, as Docker since 20.10.14. Root keeps the defaults through
/// `execve`; any other user loses them at `setresuid` and can regain at
/// most the defaults through a setuid binary or file capabilities.
fn drop_capabilities(err: RawFd, args: &ChildArgs) {
    let (drops, len) = &args.drops;
    for &cap in drops.iter().take(*len) {
        let set = CapabilitySet::from_bits_retain(1u64.checked_shl(cap).unwrap_or(0));
        match rustix::thread::remove_capability_from_bounding_set(set) {
            Ok(()) => {}
            // Past the kernel's last capability.
            Err(Errno::INVAL) => break,
            Err(e) => fail(err, Step::BoundingSet, e),
        }
    }
    match rustix::thread::clear_ambient_capability_set() {
        // Kernels before 4.3 have no ambient set.
        Ok(()) | Err(Errno::INVAL) => {}
        Err(e) => fail(err, Step::AmbientSet, e),
    }
    let current = check(
        err,
        Step::CapabilitySets,
        rustix::thread::capabilities(None),
    );
    let keep = current.permitted & CapabilitySet::from_bits_retain(args.keep);
    check(
        err,
        Step::CapabilitySets,
        rustix::thread::set_capabilities(
            None,
            CapabilitySets {
                effective: keep,
                permitted: keep,
                inheritable: CapabilitySet::empty(),
            },
        ),
    );
}

fn set_stdio(err: RawFd, stdio: &[ChildStdio; 3]) {
    for (target, how) in (0..).zip(stdio) {
        let source = match *how {
            ChildStdio::Inherit => continue,
            ChildStdio::Fd(fd) => fd,
            ChildStdio::Null => {
                let null = check(
                    err,
                    Step::Stdio,
                    rustix::fs::openat(
                        CWD,
                        c"/dev/null",
                        OFlags::RDWR | OFlags::CLOEXEC,
                        Mode::empty(),
                    ),
                );
                let raw = null.as_raw_fd();
                if raw == target {
                    // Already in place; only the close-on-exec flag must go.
                    check(
                        err,
                        Step::Stdio,
                        rustix::io::fcntl_setfd(&null, rustix::io::FdFlags::empty()),
                    );
                    std::mem::forget(null);
                    continue;
                }
                dup_to(err, raw, target);
                continue;
            }
        };
        dup_to(err, source, target);
    }
}

fn dup_to(err: RawFd, source: RawFd, target: RawFd) {
    // SAFETY: plain syscall on descriptor numbers; `dup2` clears
    // close-on-exec on `target`.
    if unsafe { libc::dup2(source, target) } < 0 {
        fail(err, Step::Stdio, last_errno());
    }
}

/// The umask, signal dispositions and mask, and descriptors a container
/// process starts with.
fn reset_process_state() {
    // Files the command creates must not depend on ranikust's umask.
    rustix::process::umask(Mode::from_raw_mode(0o022));
    // SAFETY: zeroed `sigaction` is `SIG_DFL` with an empty mask and no
    // flags; resetting a disposition to the default is async-signal-safe.
    // Rust's runtime ignores SIGPIPE, and ignored signals survive `execve`.
    unsafe {
        let mut dfl: libc::sigaction = std::mem::zeroed();
        dfl.sa_sigaction = libc::SIG_DFL;
        libc::sigaction(libc::SIGPIPE, &raw const dfl, std::ptr::null_mut());
        let mut none: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&raw mut none);
        libc::sigprocmask(libc::SIG_SETMASK, &raw const none, std::ptr::null_mut());
    }
    // Only stdio crosses `execve` (threat T7): everything else becomes
    // close-on-exec, whoever opened it.
    // SAFETY: plain syscall on a descriptor range.
    let r = unsafe {
        libc::syscall(
            libc::SYS_close_range,
            3u32,
            u32::MAX,
            libc::CLOSE_RANGE_CLOEXEC,
        )
    };
    if r != 0 {
        // Linux before 5.11: mark each open descriptor one by one.
        let limit = rustix::process::getrlimit(rustix::process::Resource::Nofile)
            .current
            .unwrap_or(1 << 16)
            .min(1 << 16);
        let limit = RawFd::try_from(limit).unwrap_or(1 << 16);
        for fd in 3..limit {
            // SAFETY: `F_GETFD`/`F_SETFD` on a descriptor number; a closed
            // one fails with EBADF and is skipped.
            unsafe {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags >= 0 && flags & libc::FD_CLOEXEC == 0 {
                    libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
                }
            }
        }
    }
}

/// Tries each candidate path, as `execvp`: a missing one moves on, a
/// permission error is remembered, any other error is final.
fn exec(err: RawFd, args: &ChildArgs, ptrs: &ExecPtrs<'_>) -> ! {
    let mut denied = false;
    for candidate in &args.candidates {
        // SAFETY: `candidate` is NUL-terminated; `argv` and `envp` are
        // NULL-terminated arrays of pointers into `args`, which outlives the
        // call. `execve` returns only on failure.
        unsafe { libc::execve(candidate.as_ptr(), ptrs.argv.as_ptr(), ptrs.envp.as_ptr()) };
        let e = last_errno();
        match e.raw_os_error() {
            libc::ENOENT | libc::ENOTDIR => {}
            libc::EACCES => denied = true,
            _ => fail(err, Step::Exec, e),
        }
    }
    fail(
        err,
        Step::Exec,
        if denied { Errno::ACCESS } else { Errno::NOENT },
    )
}

fn last_errno() -> Errno {
    // Reads `errno`; builds no heap value.
    Errno::from_raw_os_error(
        std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO),
    )
}

pub(crate) fn check<T>(err: RawFd, step: Step, r: rustix::io::Result<T>) -> T {
    match r {
        Ok(v) => v,
        Err(e) => fail(err, step, e),
    }
}

/// Reports `(step, errno)` on the error pipe and exits with 127.
pub(crate) fn fail(err: RawFd, step: Step, errno: Errno) -> ! {
    let record = encode_failure(step, errno.raw_os_error());
    // SAFETY: `err` is the child's write end of the error pipe, open until
    // `execve` or exit. Eight bytes are below PIPE_BUF, so the write is
    // atomic; if the parent is gone there is no one left to tell.
    let _ = rustix::io::write(unsafe { BorrowedFd::borrow_raw(err) }, &record);
    // SAFETY: `_exit` ends the process at once, without atexit handlers,
    // destructors or flushing, none of which are safe here.
    unsafe { libc::_exit(127) }
}

/// The 8-byte failure record: step code and errno, little-endian.
pub(crate) fn encode_failure(step: Step, errno: i32) -> [u8; 8] {
    let s = step.code().to_le_bytes();
    let e = errno.to_le_bytes();
    [s[0], s[1], s[2], s[3], e[0], e[1], e[2], e[3]]
}

/// The step and errno of a failure record; `None` for an unknown step.
pub(crate) fn decode_failure(record: [u8; 8]) -> Option<(Step, i32)> {
    let [s0, s1, s2, s3, e0, e1, e2, e3] = record;
    let step = Step::from_code(u32::from_le_bytes([s0, s1, s2, s3]))?;
    Some((step, i32::from_le_bytes([e0, e1, e2, e3])))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fd_paths() {
        for (fd, want) in [
            (0, "/proc/self/fd/0"),
            (7, "/proc/self/fd/7"),
            (1234, "/proc/self/fd/1234"),
            (RawFd::MAX, "/proc/self/fd/2147483647"),
        ] {
            let p = FdPath::new(fd);
            assert_eq!(p.as_cstr().unwrap().to_str().unwrap(), want);
        }
    }

    #[test]
    fn failure_records_round_trip() {
        let rec = encode_failure(Step::PivotRoot, libc::EPERM);
        assert_eq!(decode_failure(rec), Some((Step::PivotRoot, libc::EPERM)));
        assert_eq!(decode_failure([0xff; 8]), None);
    }

    #[test]
    fn exec_pointer_arrays_are_null_terminated() {
        let args = ChildArgs {
            root_dir: c"/r".into(),
            mounts: Vec::new(),
            root_entry: RootEntry::PivotRoot,
            hostname: Vec::new(),
            loopback: false,
            drops: ([0; 64], 0),
            keep: 0,
            uid: Uid::ROOT,
            gid: Gid::ROOT,
            groups: Vec::new(),
            workdir: c"/".into(),
            stdio: [ChildStdio::Inherit; 3],
            argv: vec![c"sh".into(), c"-c".into()],
            envp: vec![c"A=1".into()],
            candidates: Vec::new(),
        };
        let ptrs = ExecPtrs::new(&args);
        assert_eq!(ptrs.argv.len(), 3);
        assert!(ptrs.argv[2].is_null());
        assert_eq!(ptrs.envp.len(), 2);
        assert!(ptrs.envp[1].is_null());
        // SAFETY: the pointer comes from a live CString of `args`.
        assert_eq!(unsafe { CStr::from_ptr(ptrs.argv[1]) }, c"-c");
    }
}
