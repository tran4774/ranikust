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
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};

use rustix::fs::{CWD, FileType, Mode, OFlags, PROC_SUPER_MAGIC, RawDir, ResolveFlags};
use rustix::io::Errno;
use rustix::mount::{MountFlags, MountPropagationFlags, UnmountFlags};
use rustix::process::{Gid, Signal, Uid};
use rustix::thread::{CapabilitySet, CapabilitySets};

use crate::error::Step;
use crate::plan::{
    DEV_NODES, DEV_NULL, DEV_SYMLINKS, DEV_TMPFS, DEVPTS, MASKED_PATHS, READONLY_PATHS, SHM,
};

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
/// sources are host paths of files ranikust owns, which the child opens in
/// its own mount namespace: a bind refuses (`EINVAL`) a source from another
/// namespace, such as a descriptor the parent opened.
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
        /// A regular file.
        source: CString,
        target: CString,
    },
    Bind {
        source: CString,
        /// The source is a directory, else a regular file.
        dir: bool,
        target: CString,
        /// Flags of the remount that follows the bind: `nodev`, and
        /// read-only when asked.
        remount: MountFlags,
    },
}

/// A secret file the child writes on its tmpfs, bound at every target of
/// the `RUN` that mounts it.
pub(crate) struct ChildSecret {
    /// The file's name on the tmpfs.
    pub(crate) name: CString,
    pub(crate) data: std::sync::Arc<[u8]>,
    pub(crate) uid: Uid,
    pub(crate) gid: Gid,
    pub(crate) mode: Mode,
    pub(crate) targets: Vec<CString>,
}

impl std::fmt::Debug for ChildSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildSecret")
            .field("name", &self.name)
            .field("data", &format_args!("<{} bytes hidden>", self.data.len()))
            .field("targets", &self.targets)
            .finish_non_exhaustive()
    }
}

/// The secrets of a `RUN` and the tmpfs they are written on.
#[derive(Debug)]
pub(crate) struct ChildSecrets {
    /// A host directory of this `RUN` (its journal's scratch directory,
    /// whose files are already bound) the tmpfs is mounted on, in the
    /// child's own mount namespace, then detached.
    pub(crate) dir: CString,
    /// The tmpfs options: `size=` and `mode=0700`.
    pub(crate) options: CString,
    pub(crate) files: Vec<ChildSecret>,
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
    /// Flags of the remount of the root's bind: `nodev`, the rest kept.
    pub(crate) root_remount: MountFlags,
    pub(crate) mounts: Vec<ChildMount>,
    /// `None` without secrets.
    pub(crate) secrets: Option<ChildSecrets>,
    /// Flags of the read-only remount of each [`DEV_NODES`] bind.
    pub(crate) dev_remount: [MountFlags; DEV_NODES.len()],
    pub(crate) root_entry: RootEntry,
    pub(crate) hostname: Vec<u8>,
    /// Bring up `lo` in the network namespace the clone created
    /// (`--network=none` where the process has `CAP_NET_ADMIN`).
    pub(crate) loopback: bool,
    /// A pidfd of the [`hold_network`] process whose network namespace,
    /// owned by a user namespace and with `lo` already up, the child joins
    /// instead (`--network=none` without `CAP_NET_ADMIN`, ADR-0033).
    pub(crate) netns: Option<RawFd>,
    /// The seccomp filter installed before the capability drop: with
    /// `netns`, [`crate::seccomp::userns_network`].
    pub(crate) seccomp: Option<Vec<libc::sock_filter>>,
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
    /// The pid of the process that cloned the child, as the outer procfs
    /// numbers it (its `/proc/self/stat`).
    pub(crate) parent: i32,
}

/// What the child checks its parent with: its own `stat` file in the outer
/// procfs, opened before entering the root, and the pid the parent must
/// have there. A `clone` child reparented after its parent died shows
/// another parent pid, at once and whatever other children exist.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ParentCheck {
    stat: RawFd,
    parent: i32,
}

impl ParentCheck {
    /// Opens `/proc/self/stat`, numbered 3 or higher so that stdio never
    /// overwrites it, close-on-exec. Call before entering the root.
    pub(crate) fn open(err: RawFd, parent: i32) -> Self {
        let fd = check(
            err,
            Step::ParentGone,
            rustix::fs::openat(
                CWD,
                c"/proc/self/stat",
                OFlags::RDONLY | OFlags::CLOEXEC,
                Mode::empty(),
            ),
        );
        let fd = if fd.as_raw_fd() < 3 {
            check(
                err,
                Step::ParentGone,
                rustix::io::fcntl_dupfd_cloexec(&fd, 3),
            )
        } else {
            fd
        };
        // Open until `execve` closes it.
        Self {
            stat: fd.into_raw_fd(),
            parent,
        }
    }
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
    let parent = ParentCheck::open(err, fds.parent);
    arm_death_signal(err, parent);
    // First, so that `sysfs` below is mounted for this namespace.
    if let Some(fd) = args.netns {
        // SAFETY: `setns` on a pidfd the parent keeps open across the clone.
        if unsafe { libc::setns(fd, libc::CLONE_NEWNET) } != 0 {
            fail(err, Step::JoinNetwork, last_errno());
        }
    }

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
    // No device node in the root opens: neither one from a layer nor one a
    // `RUN` creates with `mknod`. Docker gets this from its device cgroup.
    check(
        err,
        Step::Remount,
        rustix::mount::mount_remount(root_dir, args.root_remount, c""),
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
        mount_one(err, root.as_fd(), &args.dev_remount, m);
    }
    if let Some(s) = &args.secrets {
        mount_secrets(err, root.as_fd(), s);
    }
    protect_kernel_files(err, root.as_fd(), args.dev_remount[DEV_NULL]);

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
            // The old root stays mounted underneath; `..` stops at the
            // moved root only because the probe chose this entry where `/`
            // is a mount root. Root keeps CAP_SYS_CHROOT [open: Q-077].
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
    // After the namespace is joined and every mount made, which the filter
    // would refuse.
    if let Some(f) = &args.seccomp {
        install_filter(err, f);
    }

    drop_capabilities(err, args);
    switch_user_and_exec(err, parent, args, ptrs)
}

/// Arms `PR_SET_PDEATHSIG(SIGKILL)`, then checks that the process that
/// cloned the child did not exit before it was armed. `getppid` is 0 in a
/// new PID namespace, so the parent pid is read from the outer procfs.
pub(crate) fn arm_death_signal(err: RawFd, parent: ParentCheck) {
    check(
        err,
        Step::DeathSignal,
        rustix::process::set_parent_process_death_signal(Some(Signal::KILL)),
    );
    // SAFETY: `stat` is open in the child until `execve` closes it.
    let stat = unsafe { BorrowedFd::borrow_raw(parent.stat) };
    // The parent pid is the fourth field, within the first 256 bytes: the
    // pid, the name (16 bytes, 64 for kernel threads) and the state come
    // before it.
    let mut buf = [0u8; 256];
    let len = loop {
        match rustix::io::pread(stat, &mut buf, 0) {
            Ok(n) => break n,
            Err(Errno::INTR) => {}
            Err(e) => fail(err, Step::ParentGone, e),
        }
    };
    match buf.get(..len).and_then(stat_pid_and_ppid) {
        Some((_, ppid)) if ppid == parent.parent => {}
        Some(_) => fail(err, Step::ParentGone, Errno::SRCH),
        None => fail(err, Step::ParentGone, Errno::INVAL),
    }
}

/// The pid and the parent pid of a `/proc/<pid>/stat` line, or of its first
/// bytes: `pid (name) state ppid …`; the name may hold spaces and `)`.
pub(crate) fn stat_pid_and_ppid(stat: &[u8]) -> Option<(i32, i32)> {
    let open = stat.iter().position(|&b| b == b'(')?;
    let close = stat.iter().rposition(|&b| b == b')')?;
    let pid = parse_fd(stat.get(..open)?.strip_suffix(b" ")?)?;
    // ") S 1234 …"
    let rest = stat.get(close + 1..)?.strip_prefix(b" ")?;
    let rest = rest.get(1..)?.strip_prefix(b" ")?;
    let end = rest.iter().position(|&b| b == b' ').unwrap_or(rest.len());
    Some((pid, parse_fd(rest.get(..end)?)?))
}

/// A new session and session keyring, `WORKDIR`, the `USER` ids, the death
/// signal again, stdio, process state, then `execve`.
pub(crate) fn switch_user_and_exec(
    err: RawFd,
    parent: ParentCheck,
    args: &ChildArgs,
    ptrs: &ExecPtrs<'_>,
) -> ! {
    // No controlling terminal: a `RUN` must not read the operator's keys or
    // push input into the operator's shell (`TIOCSTI`). Also a process
    // group of its own, which `Direct` kills afterwards.
    check(err, Step::Session, rustix::process::setsid().map(drop));
    // Still as root: the keyring counts against root's key quota, and the
    // process keeps possessing it after the switch.
    join_session_keyring(err);
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
    // A change of uid or gid cleared the death signal (`commit_creds`).
    arm_death_signal(err, parent);

    set_stdio(err, &args.stdio);
    reset_process_state();
    exec(err, args, ptrs)
}

fn mount_one(
    err: RawFd,
    root: BorrowedFd<'_>,
    dev_remount: &[MountFlags; DEV_NODES.len()],
    m: &ChildMount,
) {
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
            // `nodev`: the nodes below are binds, separate mounts that keep
            // the outer `/dev`'s flags; a node created here does not open.
            let flags = MountFlags::NOSUID | MountFlags::NODEV | MountFlags::STRICTATIME;
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
            populate_dev(err, dev.as_fd(), dev_remount);
        }
        ChildMount::File { source, target } => {
            let s = open_source(err, Step::BindFile, source, false);
            let t = resolve(err, root, target);
            check(
                err,
                Step::BindFile,
                rustix::mount::mount_bind(fd_path(err, &s).path(), fd_path(err, &t).path()),
            );
        }
        ChildMount::Bind {
            source,
            dir,
            target,
            remount,
        } => {
            let s = open_source(err, Step::BindMount, source, *dir);
            let t = resolve(err, root, target);
            check(
                err,
                Step::BindMount,
                rustix::mount::mount_bind(fd_path(err, &s).path(), fd_path(err, &t).path()),
            );
            drop(t);
            // Resolved again, it is the bind, not the directory under it.
            let t = resolve(err, root, target);
            check(
                err,
                Step::Remount,
                rustix::mount::mount_remount(fd_path(err, &t).path(), *remount, c""),
            );
        }
    }
}

/// BuildKit's secret mount (`secretmount_unix.go`): a private tmpfs
/// (`nodev`, `nosuid`, `noexec`, mode 0700), each secret written there as a
/// new file, given its owner and mode, bound read-only at its target; then
/// the tmpfs is detached, kept alive by the binds alone. The bytes never
/// reach a disk, and the tmpfs vanishes with the `RUN`'s mount namespace
/// whatever happens to ranikust.
fn mount_secrets(err: RawFd, root: BorrowedFd<'_>, secrets: &ChildSecrets) {
    let step = Step::Secret;
    let dir = secrets.dir.as_c_str();
    check(
        err,
        step,
        rustix::mount::mount(
            c"tmpfs",
            dir,
            c"tmpfs",
            MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC,
            Some(secrets.options.as_c_str()),
        ),
    );
    let d = check(
        err,
        step,
        rustix::fs::openat(
            CWD,
            dir,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ),
    );
    for s in &secrets.files {
        let f = check(
            err,
            step,
            rustix::fs::openat(
                &d,
                s.name.as_c_str(),
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            ),
        );
        let mut rest: &[u8] = &s.data;
        while !rest.is_empty() {
            match rustix::io::write(&f, rest) {
                Ok(0) => fail(err, step, Errno::NOSPC),
                Ok(n) => rest = rest.get(n..).unwrap_or_default(),
                Err(Errno::INTR) => {}
                Err(e) => fail(err, step, e),
            }
        }
        check(err, step, rustix::fs::fchown(&f, Some(s.uid), Some(s.gid)));
        check(err, step, rustix::fs::fchmod(&f, s.mode));
        for target in &s.targets {
            let t = resolve(err, root, target);
            check(
                err,
                step,
                rustix::mount::mount_bind(fd_path(err, &f).path(), fd_path(err, &t).path()),
            );
            drop(t);
            // Resolved again, it is the bind, not the stub under it.
            let t = resolve(err, root, target);
            check(
                err,
                Step::Remount,
                rustix::mount::mount_remount(
                    fd_path(err, &t).path(),
                    MountFlags::BIND
                        | MountFlags::RDONLY
                        | MountFlags::NOSUID
                        | MountFlags::NODEV
                        | MountFlags::NOEXEC,
                    c"",
                ),
            );
        }
    }
    drop(d);
    check(err, step, rustix::mount::unmount(dir, UnmountFlags::DETACH));
}

/// A mount source as an `O_PATH` descriptor in the child's own mount
/// namespace, so that the bind is of the file checked here: a symlink as
/// the last component is refused, and the type must be the one expected.
fn open_source(err: RawFd, step: Step, source: &CStr, dir: bool) -> OwnedFd {
    let fd = check(
        err,
        step,
        rustix::fs::openat(
            CWD,
            source,
            OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ),
    );
    let mode = check(err, step, rustix::fs::fstat(&fd)).st_mode;
    match (FileType::from_raw_mode(mode), dir) {
        (FileType::Directory, true) | (FileType::RegularFile, false) => fd,
        (FileType::Symlink, _) => fail(err, step, Errno::LOOP),
        (_, true) => fail(err, step, Errno::NOTDIR),
        (_, false) => fail(err, step, Errno::INVAL),
    }
}

/// Docker's read-only and masked kernel files (moby
/// `daemon/pkg/oci/defaults.go`): writing a sysctl needs only uid 0, and
/// `/proc/sys/kernel/core_pattern` or `/proc/sysrq-trigger` reach the host
/// kernel. A path the kernel does not have is skipped.
fn protect_kernel_files(err: RawFd, root: BorrowedFd<'_>, null_remount: MountFlags) {
    for path in READONLY_PATHS {
        let Some(t) = resolve_existing(err, root, path) else {
            continue;
        };
        let p = fd_path(err, &t);
        check(
            err,
            Step::ReadonlyPath,
            rustix::mount::mount_bind(p.path(), p.path()),
        );
        drop(t);
        let Some(t) = resolve_existing(err, root, path) else {
            fail(err, Step::ReadonlyPath, Errno::NOENT)
        };
        let flags = MountFlags::BIND
            | MountFlags::RDONLY
            | MountFlags::NOSUID
            | MountFlags::NODEV
            | MountFlags::NOEXEC;
        check(
            err,
            Step::ReadonlyPath,
            rustix::mount::mount_remount(fd_path(err, &t).path(), flags, c""),
        );
    }
    for path in MASKED_PATHS {
        let Some(t) = resolve_existing(err, root, path) else {
            continue;
        };
        let mode = check(err, Step::MaskPath, rustix::fs::fstat(&t)).st_mode;
        let target = fd_path(err, &t);
        if FileType::from_raw_mode(mode) == FileType::Directory {
            let flags =
                MountFlags::RDONLY | MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC;
            check(
                err,
                Step::MaskPath,
                rustix::mount::mount(c"tmpfs", target.path(), c"tmpfs", flags, None::<&CStr>),
            );
            continue;
        }
        // Still the outer `/dev/null`, as the root is not entered yet: the
        // host's inode, so read-only as the `/dev` nodes are.
        check(
            err,
            Step::MaskPath,
            rustix::mount::mount_bind(c"/dev/null", target.path()),
        );
        drop(t);
        let Some(t) = resolve_existing(err, root, path) else {
            fail(err, Step::MaskPath, Errno::NOENT)
        };
        // The flags of the `/dev/null` node bind: read-only, and the outer
        // mount's locked flags kept.
        check(
            err,
            Step::MaskPath,
            rustix::mount::mount_remount(fd_path(err, &t).path(), null_remount, c""),
        );
    }
}

/// A tmpfs `/dev`: the device nodes bound from the outer `/dev`, `pts`,
/// `shm` and the symlinks. Each node bind is remounted read-only: the bind
/// is the host's own inode, whose mode and owner uid 0 in a `RUN` could
/// otherwise change; reads and writes of a device are not affected.
fn populate_dev(err: RawFd, dev: BorrowedFd<'_>, remount: &[MountFlags; DEV_NODES.len()]) {
    for ((name, source), flags) in DEV_NODES.into_iter().zip(remount) {
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
            Ok(()) => {
                drop(node);
                // Opened again by name, it is the bind, not the file under it.
                let bound = check(
                    err,
                    Step::DevNode,
                    rustix::fs::openat(
                        dev,
                        name,
                        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    ),
                );
                check(
                    err,
                    Step::Remount,
                    rustix::mount::mount_remount(fd_path(err, &bound).path(), *flags, c""),
                );
            }
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

/// As [`resolve`], but `None` when the path does not exist.
fn resolve_existing(err: RawFd, root: BorrowedFd<'_>, target: &CStr) -> Option<OwnedFd> {
    let mut attempts = 0;
    loop {
        match rustix::fs::openat2(
            root,
            target,
            OFlags::PATH | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::IN_ROOT | ResolveFlags::NO_MAGICLINKS,
        ) {
            Ok(fd) => return Some(fd),
            Err(Errno::NOENT) => return None,
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

/// The byte a [`hold_network`] or [`try_join`] process writes when it is
/// ready: anything else, end-of-file included, is a failure.
pub(crate) const READY: u8 = b'R';

/// The process that holds the network namespace of a `--network=none`
/// `RUN` without `CAP_NET_ADMIN`, under `--allow network.none-userns`
/// (ADR-0033): it creates a user namespace and a network namespace owned
/// by it, where it has `CAP_NET_ADMIN`, brings up `lo`, writes [`READY`] on
/// `err` and waits to be killed. The `RUN` child joins the namespace
/// through its pidfd and stays in the build's user namespace, under the
/// filter of [`crate::seccomp`]. With `userns` false it creates the network
/// namespace alone: the probe that tells whether the clone can do it itself.
///
/// # Safety
///
/// Call only in the child of a `clone` without `CLONE_VM`, right after it
/// returned 0: everything here is a plain syscall, with no allocation.
pub(crate) unsafe fn hold_network(err: RawFd, parent: i32, userns: bool) -> ! {
    check(
        err,
        Step::DeathSignal,
        rustix::process::set_parent_process_death_signal(Some(Signal::KILL)),
    );
    // Not in a new PID namespace: `getppid` is the cloning process, unless it
    // died before the signal was armed.
    if rustix::process::getppid().map_or(0, |p| p.as_raw_nonzero().get()) != parent {
        fail(err, Step::ParentGone, Errno::SRCH);
    }
    // No core dump and no ptrace by its uid's other processes.
    // SAFETY: plain `prctl`.
    unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };
    // A forked copy of every descriptor of the build, stdio included:
    // closed, so that this process, which never executes, holds no lock,
    // pipe end or terminal of it.
    close_all_but(err);
    let mut flags = libc::CLONE_NEWNET;
    if userns {
        flags |= libc::CLONE_NEWUSER;
    }
    // SAFETY: plain syscall; the process is single-threaded (a clone).
    if unsafe { libc::unshare(flags) } != 0 {
        fail(err, Step::NetNamespace, last_errno());
    }
    loopback_up(err);
    ready_and_close(err);
    loop {
        // SAFETY: `pause` waits for the parent's SIGKILL or the death signal.
        unsafe { libc::pause() };
    }
}

/// Joins the network namespace of the [`hold_network`] process `pidfd`
/// refers to, then exits: the probe's check that `setns` on a pidfd works
/// here, as the `RUN` child will need.
///
/// # Safety
///
/// As [`hold_network`].
pub(crate) unsafe fn try_join(err: RawFd, pidfd: RawFd) -> ! {
    // SAFETY: `setns` on a pidfd the parent keeps open across the clone.
    if unsafe { libc::setns(pidfd, libc::CLONE_NEWNET) } != 0 {
        fail(err, Step::JoinNetwork, last_errno());
    }
    ready_and_close(err);
    // SAFETY: `_exit` without handlers, as in `fail`.
    unsafe { libc::_exit(0) }
}

/// Writes [`READY`] on `err` and closes it.
fn ready_and_close(err: RawFd) {
    // SAFETY: `err` is this process's write end of the parent's pipe; one
    // byte is written atomically.
    let fd = unsafe { BorrowedFd::borrow_raw(err) };
    if rustix::io::write(fd, &[READY]) != Ok(1) {
        fail(err, Step::NetNamespace, Errno::IO);
    }
    // SAFETY: closing that write end, which nothing uses after.
    unsafe { libc::close(err) };
}

/// Closes every descriptor except `keep`, from 0: with `close_range`, else
/// those `/proc/self/fd` lists, else every number up to the hard limit.
fn close_all_but(keep: RawFd) {
    let k = u32::try_from(keep).unwrap_or(u32::MAX);
    let mut ok = true;
    if k > 0 {
        // SAFETY: plain syscall on a descriptor range.
        ok &= unsafe { libc::syscall(libc::SYS_close_range, 0u32, k - 1, 0u32) } == 0;
    }
    if k < u32::MAX {
        // SAFETY: as above.
        ok &= unsafe { libc::syscall(libc::SYS_close_range, k + 1, u32::MAX, 0u32) } == 0;
    }
    if ok || close_listed(keep) {
        return;
    }
    let limit = rustix::process::getrlimit(rustix::process::Resource::Nofile)
        .maximum
        .unwrap_or(1 << 20)
        .min(1 << 20);
    let limit = RawFd::try_from(limit).unwrap_or(1 << 20);
    for fd in 0..=limit {
        if fd != keep {
            // SAFETY: closing a descriptor number; a closed one fails with
            // EBADF.
            unsafe { libc::close(fd) };
        }
    }
}

/// Closes every descriptor `/proc/self/fd` lists except `keep`, passing
/// again until one pass finds none; `false` if the list cannot be read.
fn close_listed(keep: RawFd) -> bool {
    loop {
        let Ok(dir) = rustix::fs::openat(
            CWD,
            c"/proc/self/fd",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        ) else {
            return false;
        };
        match rustix::fs::fstatfs(&dir) {
            Ok(st) if st.f_type == PROC_SUPER_MAGIC => {}
            _ => return false,
        }
        let own = dir.as_raw_fd();
        let mut closed = 0;
        let mut buf = [const { MaybeUninit::<u8>::uninit() }; 4096];
        let mut entries = RawDir::new(&dir, &mut buf);
        while let Some(entry) = entries.next() {
            let Ok(entry) = entry else {
                return false;
            };
            if let Some(fd) = parse_fd(entry.file_name().to_bytes())
                && fd != keep
                && fd != own
            {
                // SAFETY: closing a listed descriptor number.
                unsafe { libc::close(fd) };
                closed += 1;
            }
        }
        if closed == 0 {
            return true;
        }
    }
}

/// Installs the seccomp filter of a `RUN` that joined a held network
/// namespace ([`crate::seccomp`]). Without `no_new_privs`: the child still
/// has `CAP_SYS_ADMIN` here, and a setuid binary of the `RUN` must keep
/// working.
fn install_filter(err: RawFd, filter: &[libc::sock_filter]) {
    let prog = libc::sock_fprog {
        len: u16::try_from(filter.len()).unwrap_or(u16::MAX),
        filter: filter.as_ptr().cast_mut(),
    };
    // SAFETY: `seccomp(SECCOMP_SET_MODE_FILTER)` reads the program, which
    // lives across the call.
    let r = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            0u32,
            &raw const prog,
        )
    };
    if r != 0 {
        fail(err, Step::Seccomp, last_errno());
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

/// A new, empty session keyring, so that a `RUN` cannot use the keys of
/// ranikust's session keyring (threat T11), as runc does. Where `keyctl` is
/// missing or refused (Docker's seccomp profile), the `RUN` cannot reach
/// any keyring either.
fn join_session_keyring(err: RawFd) {
    // `KEYCTL_JOIN_SESSION_KEYRING` from `linux/keyctl.h`, a stable ABI.
    const KEYCTL_JOIN_SESSION_KEYRING: libc::c_long = 1;
    let anonymous: libc::c_long = 0;
    // SAFETY: a plain syscall; a null name asks for an anonymous keyring.
    let r = unsafe { libc::syscall(libc::SYS_keyctl, KEYCTL_JOIN_SESSION_KEYRING, anonymous) };
    if r < 0 {
        let e = last_errno();
        if !matches!(e, Errno::NOSYS | Errno::PERM | Errno::ACCESS) {
            fail(err, Step::SessionKeyring, e);
        }
    }
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
    if r != 0 && !cloexec_listed() {
        // Linux before 5.11 without a readable `/proc/self/fd`: mark each
        // descriptor number one by one, up to a bound.
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

/// Marks every descriptor listed in `/proc/self/fd` from 3 up
/// close-on-exec, with a buffer on the stack; `false` if the list cannot be
/// read.
fn cloexec_listed() -> bool {
    let Ok(dir) = rustix::fs::openat(
        CWD,
        c"/proc/self/fd",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    ) else {
        return false;
    };
    // After entering the root, `/proc` is the image's path: only the proc
    // mounted there may be trusted to list the descriptors.
    match rustix::fs::fstatfs(&dir) {
        Ok(st) if st.f_type == PROC_SUPER_MAGIC => {}
        _ => return false,
    }
    let mut buf = [const { MaybeUninit::<u8>::uninit() }; 4096];
    let mut entries = RawDir::new(&dir, &mut buf);
    while let Some(entry) = entries.next() {
        let Ok(entry) = entry else {
            return false;
        };
        let Some(fd) = parse_fd(entry.file_name().to_bytes()) else {
            continue;
        };
        if fd >= 3 {
            // SAFETY: `F_GETFD`/`F_SETFD` on a descriptor number; one closed
            // meanwhile fails with EBADF and is skipped.
            unsafe {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags >= 0 && flags & libc::FD_CLOEXEC == 0 {
                    libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
                }
            }
        }
    }
    true
}

/// A descriptor number from its decimal name; `None` for `.` and `..`.
pub(crate) fn parse_fd(name: &[u8]) -> Option<RawFd> {
    if name.is_empty() {
        return None;
    }
    let mut n: RawFd = 0;
    for &b in name {
        if !b.is_ascii_digit() {
            return None;
        }
        n = n.checked_mul(10)?.checked_add(RawFd::from(b - b'0'))?;
    }
    Some(n)
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
    fn stat_lines_give_pid_and_parent_pid() {
        assert_eq!(
            stat_pid_and_ppid(b"42 (sh) S 7 42 42 0 -1 4194560 100"),
            Some((42, 7))
        );
        // A name with spaces and parentheses.
        assert_eq!(stat_pid_and_ppid(b"9 (a) b (c)) R 1 9 9"), Some((9, 1)));
        // Cut after the parent pid, as a 256-byte read may be.
        assert_eq!(stat_pid_and_ppid(b"5 (x) S 3"), Some((5, 3)));
        assert_eq!(stat_pid_and_ppid(b"5 (x) S"), None);
        assert_eq!(stat_pid_and_ppid(b"garbage"), None);
        let own = std::fs::read("/proc/self/stat").unwrap();
        let (pid, ppid) = stat_pid_and_ppid(&own).unwrap();
        assert_eq!(pid, rustix::process::getpid().as_raw_nonzero().get());
        assert_eq!(
            Some(ppid),
            rustix::process::getppid().map(|p| p.as_raw_nonzero().get())
        );
    }

    #[test]
    fn fd_names_parse_without_allocating() {
        assert_eq!(parse_fd(b"0"), Some(0));
        assert_eq!(parse_fd(b"1234"), Some(1234));
        assert_eq!(parse_fd(b"."), None);
        assert_eq!(parse_fd(b".."), None);
        assert_eq!(parse_fd(b""), None);
        assert_eq!(parse_fd(b"99999999999"), None);
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
            root_remount: MountFlags::BIND,
            mounts: Vec::new(),
            secrets: None,
            dev_remount: [MountFlags::BIND; DEV_NODES.len()],
            root_entry: RootEntry::PivotRoot,
            hostname: Vec::new(),
            loopback: false,
            netns: None,
            seccomp: None,
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
