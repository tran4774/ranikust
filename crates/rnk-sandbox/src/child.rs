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

use rustix::fs::{
    AtFlags, CWD, FileType, FsWord, Mode, OFlags, PROC_SUPER_MAGIC, RawDir, ResolveFlags,
    StatxFlags,
};
use rustix::io::Errno;
use rustix::mount::{
    MountFlags, MountPropagationFlags, MoveMountFlags, OpenTreeFlags, UnmountFlags,
};
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
    /// Make every mount of the root's recursive bind `nodev`, submounts
    /// included, with `mount_setattr(AT_RECURSIVE)` (kernel 5.12). `false`
    /// where the kernel lacks it: the parent then refused any root with a
    /// mount below it ([`crate::spawner`]), so the top remount covers all.
    pub(crate) root_nodev_rec: bool,
    /// `open_tree` and `move_mount` (Linux 5.2) are allowed: a bind is made
    /// detached, attached at its target and remounted through its own
    /// descriptor, so no path is looked up again once it is there. `false`
    /// where a seccomp profile refuses them: a bind target may then not
    /// pass through a symlink, and the mount found again must be the new
    /// bind ([`bind_remount`]).
    pub(crate) mount_api: bool,
    /// Without `mount_api`, room for the mount id of every bind the child
    /// remounts, sized by the parent.
    pub(crate) made: MadeMounts,
    pub(crate) mounts: Vec<ChildMount>,
    /// `None` without secrets.
    pub(crate) secrets: Option<ChildSecrets>,
    /// Flags of the read-only remount of each [`DEV_NODES`] bind.
    pub(crate) dev_remount: [MountFlags; DEV_NODES.len()],
    /// Make `/dev/pipes` in the `/dev` tmpfs: a mount targets it
    /// ([`crate::plan::dev_pipes`]).
    pub(crate) dev_pipes: bool,
    pub(crate) root_entry: RootEntry,
    pub(crate) hostname: Vec<u8>,
    /// Bring up `lo` in the network namespace the clone created
    /// (`--network=none` where the process has `CAP_NET_ADMIN`).
    pub(crate) loopback: bool,
    /// A descriptor of the network namespace [`create_network`] made, owned
    /// by a user namespace and with `lo` already up, which the child joins
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
    /// `Isolation::Direct` on a root other than `/`: `chroot` into
    /// `root_dir` first, and check that the new `/` is this directory,
    /// `(dev major, dev minor, ino)`, the one the parent checked.
    pub(crate) chroot: Option<(u32, u32, u64)>,
    /// The full sandbox: the device and inode the root's bind must show
    /// (`RunSpec::root_id`), checked on the descriptor opened after the
    /// bind; a path that now leads elsewhere fails the `RUN`.
    pub(crate) root_check: Option<(u32, u32, u64)>,
    /// `Isolation::Direct` on `/` with capabilities outside Docker's
    /// defaults in the bounding set and `CAP_SETPCAP` to drop them: the
    /// child drops them before `execve` (ADR-0051), so a `RUN` never holds
    /// `CAP_SYS_PTRACE` against ranikust.
    pub(crate) direct_drop: bool,
    /// The `RLIMIT_NOFILE` the command starts with; `None` keeps ranikust's.
    pub(crate) nofile: Option<rustix::process::Rlimit>,
    /// The `RLIMIT_CORE` the command starts with; `None` keeps ranikust's.
    pub(crate) core: Option<rustix::process::Rlimit>,
    pub(crate) stdio: [ChildStdio; 3],
    pub(crate) argv: Vec<CString>,
    pub(crate) envp: Vec<CString>,
    pub(crate) candidates: Vec<CString>,
    /// A `#!` heredoc of a `Direct` `RUN` on `/`: a sealed memfd holding
    /// the script, numbered 3 or higher, executed with `execveat` instead
    /// of `candidates`. The kernel names it `/dev/fd/<n>` to the
    /// interpreter, so it is the one descriptor besides stdio that crosses
    /// `execve` (ADR-0057).
    pub(crate) script: Option<RawFd>,
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

/// `MOUNT_ATTR_NODEV` of `<linux/mount.h>`.
pub(crate) const MOUNT_ATTR_NODEV: u64 = 0x0000_0004;

/// `struct mount_attr` of `<linux/mount.h>` (`MOUNT_ATTR_SIZE_VER0`).
#[repr(C)]
pub(crate) struct MountAttr {
    pub(crate) attr_set: u64,
    pub(crate) attr_clr: u64,
    pub(crate) propagation: u64,
    pub(crate) userns_fd: u64,
}

/// `mount_setattr(AT_FDCWD, path, AT_RECURSIVE, {attr_set: NODEV})`: 0, or
/// -1 with `errno` set. Allocates nothing, so the child may call it.
///
/// # Safety
///
/// `path` must stay valid for the call.
pub(crate) unsafe fn set_nodev_recursive(path: &std::ffi::CStr) -> libc::c_long {
    let attr = MountAttr {
        attr_set: MOUNT_ATTR_NODEV,
        attr_clr: 0,
        propagation: 0,
        userns_fd: 0,
    };
    // SAFETY: as the caller guarantees for `path`; `attr` lives on this
    // stack frame for the call and its size is passed.
    unsafe {
        libc::syscall(
            libc::SYS_mount_setattr,
            libc::AT_FDCWD,
            path.as_ptr(),
            libc::AT_RECURSIVE as libc::c_uint,
            &raw const attr,
            std::mem::size_of::<MountAttr>(),
        )
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
    close_inherited(args, fds, parent);
    early_stdio(err, &args.stdio, parent);
    // First, so that `sysfs` below is mounted for this namespace.
    if let Some(fd) = args.netns {
        // SAFETY: `setns` on a namespace descriptor the parent keeps open
        // across the clone.
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
    // The remount reaches the top mount only; a mount below the root keeps
    // its own flags unless made `nodev` too.
    if args.root_nodev_rec {
        // SAFETY: a NUL-terminated path and a `mount_attr` of the size
        // passed, both alive for the call.
        if unsafe { set_nodev_recursive(root_dir) } != 0 {
            fail(err, Step::Remount, last_errno());
        }
    }
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
    if let Some(want) = args.root_check {
        let st = check(
            err,
            Step::EnterRoot,
            rustix::fs::statx(
                &root,
                c"",
                rustix::fs::AtFlags::EMPTY_PATH,
                rustix::fs::StatxFlags::BASIC_STATS,
            ),
        );
        if (st.stx_dev_major, st.stx_dev_minor, st.stx_ino) != want {
            fail(err, Step::EnterRoot, Errno::XDEV);
        }
    }
    let (mut proc, mut sys, mut null) = (None, None, None);
    for m in &args.mounts {
        let made = mount_one(err, root.as_fd(), args, m);
        match m {
            ChildMount::Proc { .. } => proc = made,
            ChildMount::Sysfs { .. } => sys = made,
            ChildMount::Dev { .. } => null = made,
            _ => {}
        }
    }
    // Every `Null` slot gets the sandbox's `/dev/null`, or the `RUN` does
    // not start: the slot still holds a placeholder.
    match null {
        Some(null) => null_stdio(err, &args.stdio, &null),
        None if args.stdio.contains(&ChildStdio::Null) => fail(err, Step::Stdio, Errno::NOENT),
        None => {}
    }
    if let Some(s) = &args.secrets {
        mount_secrets(err, root.as_fd(), s, args);
    }
    // Under the `proc` and `sysfs` just mounted, by descriptor: a later
    // mount or a symlink of the root cannot hide a path from them.
    let (Some(proc), Some(sys)) = (proc, sys) else {
        fail(err, Step::ReadonlyPath, Errno::NOENT)
    };
    protect_kernel_files(err, &proc, &sys, args);
    drop((proc, sys));

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
            // is a mount root. Root keeps CAP_SYS_CHROOT (Q-077,
            // answered in `docs/open-questions.md`).
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
    let chdir_again = match enter_workdir(workdir) {
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
    // The switch set the dump flag to `fs.suid_dumpable`, which may be 1:
    // until `execve` the child holds a copy of ranikust's memory, which a
    // process of the same uid could then read through `/proc/<pid>/mem`.
    // `execve` sets the flag again for the command, so the `RUN` sees no
    // difference.
    check(
        err,
        Step::Uid,
        rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable),
    );
    if chdir_again {
        check(err, Step::Workdir, enter_workdir(workdir));
    }
    // A change of uid or gid cleared the death signal (`commit_creds`).
    arm_death_signal(err, parent);

    reset_process_state();
    // After the close-on-exec pass, which may count descriptors up to the
    // current limit.
    if let Some(limit) = args.nofile {
        check(
            err,
            Step::FileLimit,
            rustix::process::setrlimit(rustix::process::Resource::Nofile, limit),
        );
    }
    if let Some(limit) = args.core {
        check(
            err,
            Step::CoreLimit,
            rustix::process::setrlimit(rustix::process::Resource::Core, limit),
        );
    }
    exec(err, args, ptrs)
}

/// Changes to `workdir`, resolved in the process's root without magic
/// links: `/proc/self/fd/<n>`, `/dev/fd/<n>` or a symlink to one fails
/// (`ELOOP`) rather than leading to a directory outside the root. Then, as
/// runc since CVE-2024-21626, the new working directory must be reachable
/// from the root.
fn enter_workdir(workdir: &CStr) -> rustix::io::Result<()> {
    let root = rustix::fs::openat(
        CWD,
        c"/",
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let mut attempts = 0;
    let dir = loop {
        match rustix::fs::openat2(
            &root,
            workdir,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::IN_ROOT | ResolveFlags::NO_MAGICLINKS,
        ) {
            Ok(fd) => break fd,
            // A rename raced the lookup; the kernel asks to retry.
            Err(Errno::AGAIN) if attempts < 16 => attempts += 1,
            Err(e) => return Err(e),
        }
    };
    rustix::process::fchdir(&dir)?;
    cwd_in_root()
}

/// `getcwd(2)` names the working directory from the process's root, or
/// starts with `(unreachable)` when it is not below it. Read on the stack:
/// a path longer than the buffer fails (`ERANGE`) rather than go unchecked.
fn cwd_in_root() -> rustix::io::Result<()> {
    let mut buf = [0u8; 4096];
    // SAFETY: the kernel writes at most `buf.len()` bytes into `buf`, which
    // lives across the call.
    let r = unsafe { libc::syscall(libc::SYS_getcwd, buf.as_mut_ptr(), buf.len()) };
    if r < 0 {
        return Err(last_errno());
    }
    match buf.first() {
        Some(b'/') => Ok(()),
        _ => Err(Errno::XDEV),
    }
}

/// Performs `m`. For `proc` and `sysfs`, returns a descriptor of the new
/// mount's root, under which [`protect_kernel_files`] finds its paths.
fn mount_one(
    err: RawFd,
    root: BorrowedFd<'_>,
    args: &ChildArgs,
    m: &ChildMount,
) -> Option<OwnedFd> {
    match m {
        ChildMount::Proc { target } => Some(mount_kernel_fs(
            err,
            Step::MountProc,
            root,
            target,
            c"proc",
            MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC,
            PROC_SUPER_MAGIC,
        )),
        ChildMount::Sysfs { target } => Some(mount_kernel_fs(
            err,
            Step::MountSys,
            root,
            target,
            c"sysfs",
            MountFlags::RDONLY | MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC,
            SYSFS_MAGIC,
        )),
        ChildMount::Dev { target } => {
            let t = resolve_dir(err, Step::MountDev, root, target);
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
            // Found again, it is the tmpfs, not the directory under it:
            // the path has no symlink, and nothing else writes the root
            // while the child sets it up.
            let dev = resolve_dir(err, Step::MountDev, root, target);
            fs_is(err, Step::MountDev, &dev, TMPFS_MAGIC);
            populate_dev(err, dev.as_fd(), &args.dev_remount, args.dev_pipes);
            // Opened now, before any `RUN --mount` can cover `/dev`.
            args.stdio
                .contains(&ChildStdio::Null)
                .then(|| open_dev_null(err, dev.as_fd()))
        }
        ChildMount::File { source, target } => {
            let s = open_source(err, Step::BindFile, source, false);
            let t = resolve(err, root, target);
            check(
                err,
                Step::BindFile,
                rustix::mount::mount_bind(fd_path(err, &s).path(), fd_path(err, &t).path()),
            );
            None
        }
        ChildMount::Bind {
            source,
            dir,
            target,
            remount,
        } => {
            let s = open_source(err, Step::BindMount, source, *dir);
            let t = resolve_target(err, root, target, args.mount_api);
            bind_remount(
                err,
                Step::BindMount,
                s.as_fd(),
                t.as_fd(),
                *remount,
                args.mount_api,
                &args.made,
                &|| resolve_target(err, root, target, false),
            );
            None
        }
    }
}

/// `SYSFS_MAGIC` of `<linux/magic.h>`.
const SYSFS_MAGIC: FsWord = 0x6265_6572;
/// `TMPFS_MAGIC` of `<linux/magic.h>`.
const TMPFS_MAGIC: FsWord = 0x0102_1994;

/// Mounts the kernel filesystem `fstype` at `target`, which must be a
/// directory of the root reached without a symlink (runc requires as much
/// for `proc` and `sysfs`), and returns a descriptor of the new mount's
/// root: the path is found again, with no symlink to follow and nothing else
/// writing the root meanwhile, and the filesystem found must be `fstype`.
fn mount_kernel_fs(
    err: RawFd,
    step: Step,
    root: BorrowedFd<'_>,
    target: &CStr,
    fstype: &CStr,
    flags: MountFlags,
    magic: FsWord,
) -> OwnedFd {
    let t = resolve_dir(err, step, root, target);
    check(
        err,
        step,
        rustix::mount::mount(
            fstype,
            fd_path(err, &t).path(),
            fstype,
            flags,
            None::<&CStr>,
        ),
    );
    drop(t);
    let made = resolve_dir(err, step, root, target);
    fs_is(err, step, &made, magic);
    made
}

/// Fails `step` unless `fd` is on a filesystem of type `magic`.
fn fs_is(err: RawFd, step: Step, fd: &OwnedFd, magic: FsWord) {
    if check(err, step, rustix::fs::fstatfs(fd)).f_type != magic {
        fail(err, step, Errno::XDEV);
    }
}

/// Binds `source` onto `target` and remounts the new mount with `flags`.
///
/// With `mount_api`, nothing is looked up again: `open_tree(OPEN_TREE_CLONE)`
/// makes the bind as a detached mount, `move_mount` attaches it at `target`,
/// and the remount names that mount through its own descriptor. Resolving
/// the target again by name would cross into the new bind and could follow
/// symlinks inside it, so that the remount, `nodev` included, reached
/// another mount (the root's, `sysfs`, a `/dev` node) and left the bind
/// without it.
///
/// Without, the mount `again` finds (the target resolved without symlinks)
/// must show the source's inode on a mount other than the one `target` was
/// on, and must not be a mount this child bound before (`made`): one source
/// may be bound at two targets (a cache key mounted twice, a secret, the
/// `/dev/null` masks), and a rename by a parallel stage in a shared cache
/// could lead the lookup to the earlier bind. Else the step fails.
#[expect(
    clippy::too_many_arguments,
    reason = "the mount, its flags and the fallback's lookup and record"
)]
fn bind_remount(
    err: RawFd,
    step: Step,
    source: BorrowedFd<'_>,
    target: BorrowedFd<'_>,
    flags: MountFlags,
    mount_api: bool,
    made: &MadeMounts,
    again: &dyn Fn() -> OwnedFd,
) {
    if mount_api {
        let tree = check(
            err,
            step,
            rustix::mount::open_tree(
                source,
                c"",
                OpenTreeFlags::OPEN_TREE_CLONE
                    | OpenTreeFlags::OPEN_TREE_CLOEXEC
                    | OpenTreeFlags::AT_EMPTY_PATH,
            ),
        );
        check(
            err,
            step,
            rustix::mount::move_mount(
                &tree,
                c"",
                target,
                c"",
                MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH | MoveMountFlags::MOVE_MOUNT_T_EMPTY_PATH,
            ),
        );
        check(
            err,
            Step::Remount,
            rustix::mount::mount_remount(fd_path(err, &tree).path(), flags, c""),
        );
        return;
    }
    let want = mount_identity(err, step, source);
    let below = mount_identity(err, step, target);
    check(
        err,
        step,
        rustix::mount::mount_bind(fd_path(err, source).path(), fd_path(err, target).path()),
    );
    let bind = again();
    let got = mount_identity(err, Step::Remount, bind.as_fd());
    if got.inode != want.inode || got.mount == below.mount || made.contains(got.mount) {
        fail(err, Step::Remount, Errno::XDEV);
    }
    if !made.push(got.mount) {
        fail(err, Step::Remount, Errno::NOSPC);
    }
    check(
        err,
        Step::Remount,
        rustix::mount::mount_remount(fd_path(err, &bind).path(), flags, c""),
    );
}

/// The mount ids of the binds a child made without the mount API
/// ([`bind_remount`]), in room the parent sized before the clone: the child
/// only writes into it, never allocates.
#[derive(Debug, Default)]
pub(crate) struct MadeMounts {
    ids: Vec<std::cell::Cell<u64>>,
    len: std::cell::Cell<usize>,
}

impl MadeMounts {
    /// Room for `n` mount ids.
    pub(crate) fn with_room(n: usize) -> Self {
        Self {
            ids: vec![std::cell::Cell::new(0); n],
            len: std::cell::Cell::new(0),
        }
    }

    fn contains(&self, id: u64) -> bool {
        self.ids.iter().take(self.len.get()).any(|c| c.get() == id)
    }

    /// Records `id`; `false` when there is no room left.
    fn push(&self, id: u64) -> bool {
        let n = self.len.get();
        let Some(slot) = self.ids.get(n) else {
            return false;
        };
        slot.set(id);
        self.len.set(n + 1);
        true
    }
}

/// Which inode a descriptor names and on which mount.
struct MountIdentity {
    inode: (u32, u32, u64),
    mount: u64,
}

fn mount_identity(err: RawFd, step: Step, fd: BorrowedFd<'_>) -> MountIdentity {
    let st = check(
        err,
        step,
        rustix::fs::statx(
            fd,
            c"",
            AtFlags::EMPTY_PATH,
            StatxFlags::BASIC_STATS | StatxFlags::MNT_ID,
        ),
    );
    // Linux 5.8 fills it; without it nothing tells the mounts apart.
    if st.stx_mask & StatxFlags::MNT_ID.bits() == 0 {
        fail(err, step, Errno::NOSYS);
    }
    MountIdentity {
        inode: (st.stx_dev_major, st.stx_dev_minor, st.stx_ino),
        mount: st.stx_mnt_id,
    }
}

/// BuildKit's secret mount (`secretmount_unix.go`): a private tmpfs
/// (`nodev`, `nosuid`, `noexec`, mode 0700), each secret written there as a
/// new file, given its owner and mode, bound read-only at its target; then
/// the tmpfs is detached, kept alive by the binds alone. The bytes never
/// reach a disk, and the tmpfs vanishes with the `RUN`'s mount namespace
/// whatever happens to ranikust.
fn mount_secrets(err: RawFd, root: BorrowedFd<'_>, secrets: &ChildSecrets, args: &ChildArgs) {
    let mount_api = args.mount_api;
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
            let t = resolve_target(err, root, target, mount_api);
            bind_remount(
                err,
                step,
                f.as_fd(),
                t.as_fd(),
                MountFlags::BIND
                    | MountFlags::RDONLY
                    | MountFlags::NOSUID
                    | MountFlags::NODEV
                    | MountFlags::NOEXEC,
                mount_api,
                &args.made,
                &|| resolve_target(err, root, target, false),
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
/// kernel. Each path is found under the `proc` or `sysfs` mount just made,
/// `proc` and `sys` (no symlink, no magic link), never by its name from the
/// root, which symlinks of the image and later mounts could lead elsewhere.
/// A path missing there is one this kernel does not have, and is skipped.
fn protect_kernel_files(err: RawFd, proc: &OwnedFd, sys: &OwnedFd, args: &ChildArgs) {
    let under = |path: &'static CStr, step: Step| -> (BorrowedFd<'_>, &'static CStr) {
        match kernel_path(path) {
            Some((true, rest)) => (proc.as_fd(), rest),
            Some((false, rest)) => (sys.as_fd(), rest),
            None => fail(err, step, Errno::INVAL),
        }
    };
    for path in READONLY_PATHS {
        let step = Step::ReadonlyPath;
        let (fs, rest) = under(path, step);
        let Some(t) = beneath(err, step, fs, rest) else {
            continue;
        };
        bind_remount(
            err,
            step,
            t.as_fd(),
            t.as_fd(),
            MountFlags::BIND
                | MountFlags::RDONLY
                | MountFlags::NOSUID
                | MountFlags::NODEV
                | MountFlags::NOEXEC,
            args.mount_api,
            &args.made,
            &|| beneath(err, step, fs, rest).unwrap_or_else(|| fail(err, step, Errno::NOENT)),
        );
    }
    // Still the outer `/dev/null`, as the root is not entered yet: the
    // host's inode, so read-only as the `/dev` nodes are.
    let null = check(
        err,
        Step::MaskPath,
        rustix::fs::openat(
            CWD,
            c"/dev/null",
            OFlags::PATH | OFlags::CLOEXEC,
            Mode::empty(),
        ),
    );
    for path in MASKED_PATHS {
        let step = Step::MaskPath;
        let (fs, rest) = under(path, step);
        let Some(t) = beneath(err, step, fs, rest) else {
            continue;
        };
        let mode = check(err, step, rustix::fs::fstat(&t)).st_mode;
        if FileType::from_raw_mode(mode) == FileType::Directory {
            let flags =
                MountFlags::RDONLY | MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC;
            check(
                err,
                step,
                rustix::mount::mount(
                    c"tmpfs",
                    fd_path(err, &t).path(),
                    c"tmpfs",
                    flags,
                    None::<&CStr>,
                ),
            );
            continue;
        }
        // The flags of the `/dev/null` node bind: read-only, and the outer
        // mount's locked flags kept.
        bind_remount(
            err,
            step,
            null.as_fd(),
            t.as_fd(),
            args.dev_remount[DEV_NULL],
            args.mount_api,
            &args.made,
            &|| beneath(err, step, fs, rest).unwrap_or_else(|| fail(err, step, Errno::NOENT)),
        );
    }
}

/// A [`READONLY_PATHS`] or [`MASKED_PATHS`] entry as `(under proc, the rest
/// of the path)`: `/proc/sys` is `(true, "sys")`, `/sys/firmware`
/// `(false, "firmware")`.
pub(crate) fn kernel_path(path: &'static CStr) -> Option<(bool, &'static CStr)> {
    let bytes = path.to_bytes_with_nul();
    for (prefix, proc) in [(&b"/proc/"[..], true), (&b"/sys/"[..], false)] {
        if let Some(rest) = bytes.strip_prefix(prefix) {
            return CStr::from_bytes_with_nul(rest)
                .ok()
                .filter(|r| !r.is_empty())
                .map(|r| (proc, r));
        }
    }
    None
}

/// A tmpfs `/dev`: the device nodes bound from the outer `/dev`, `pts`,
/// `shm` and the symlinks. Each node bind is remounted read-only: the bind
/// is the host's own inode, whose mode and owner uid 0 in a `RUN` could
/// otherwise change; reads and writes of a device are not affected.
fn populate_dev(
    err: RawFd,
    dev: BorrowedFd<'_>,
    remount: &[MountFlags; DEV_NODES.len()],
    pipes: bool,
) {
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
    // The mount point of a `#!` heredoc's script directory, as BuildKit's.
    if pipes {
        check(
            err,
            Step::DevMounts,
            rustix::fs::mkdirat(dev, c"pipes", Mode::from_raw_mode(0o755)),
        );
    }
}

/// `path` under `dir` as an `O_PATH` descriptor, `None` when it does not
/// exist: `ResolveFlags` `how` and `oflags` on top of `O_PATH`,
/// `O_CLOEXEC` and no magic link. A failure is `step`'s.
fn lookup(
    err: RawFd,
    step: Step,
    dir: BorrowedFd<'_>,
    path: &CStr,
    oflags: OFlags,
    how: ResolveFlags,
) -> Option<OwnedFd> {
    let mut attempts = 0;
    loop {
        match rustix::fs::openat2(
            dir,
            path,
            OFlags::PATH | OFlags::CLOEXEC | oflags,
            Mode::empty(),
            how | ResolveFlags::NO_MAGICLINKS,
        ) {
            Ok(fd) => return Some(fd),
            Err(Errno::NOENT) => return None,
            // A rename raced the lookup; the kernel asks to retry.
            Err(Errno::AGAIN) if attempts < 16 => attempts += 1,
            Err(e) => fail(err, step, e),
        }
    }
}

/// `target` inside the root, as an `O_PATH` descriptor: symlinks are followed
/// but cannot leave the root, magic links are refused.
fn resolve(err: RawFd, root: BorrowedFd<'_>, target: &CStr) -> OwnedFd {
    let step = Step::ResolveTarget;
    lookup(
        err,
        step,
        root,
        target,
        OFlags::empty(),
        ResolveFlags::IN_ROOT,
    )
    .unwrap_or_else(|| fail(err, step, Errno::NOENT))
}

/// The target of a bind the child remounts: as [`resolve`] with
/// `follow_symlinks`, else without following any symlink (`ELOOP`).
fn resolve_target(
    err: RawFd,
    root: BorrowedFd<'_>,
    target: &CStr,
    follow_symlinks: bool,
) -> OwnedFd {
    let step = Step::ResolveTarget;
    let how = if follow_symlinks {
        ResolveFlags::IN_ROOT
    } else {
        ResolveFlags::IN_ROOT | ResolveFlags::NO_SYMLINKS
    };
    lookup(err, step, root, target, OFlags::empty(), how)
        .unwrap_or_else(|| fail(err, step, Errno::NOENT))
}

/// A directory of the root reached without any symlink: `ELOOP` for one,
/// `ENOTDIR` for anything but a directory.
fn resolve_dir(err: RawFd, step: Step, root: BorrowedFd<'_>, target: &CStr) -> OwnedFd {
    let how = ResolveFlags::IN_ROOT | ResolveFlags::NO_SYMLINKS;
    lookup(err, step, root, target, OFlags::DIRECTORY, how)
        .unwrap_or_else(|| fail(err, step, Errno::NOENT))
}

/// `path` below the kernel filesystem root `dir`, without any symlink;
/// `None` when the kernel lacks it.
fn beneath(err: RawFd, step: Step, dir: BorrowedFd<'_>, path: &CStr) -> Option<OwnedFd> {
    let how = ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS;
    lookup(err, step, dir, path, OFlags::empty(), how)
}

/// The path that names `fd`'s inode for `mount(2)`. The caller keeps `fd`
/// open while the path is used.
fn fd_path(err: RawFd, fd: impl AsFd) -> FdPath {
    let p = FdPath::new(fd.as_fd().as_raw_fd());
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

/// The byte a [`create_network`] or [`try_join`] helper sends when it is
/// ready: anything else, end-of-file included, is a failure.
pub(crate) const READY: u8 = b'R';

/// The helper that creates the network namespace of a `--network=none`
/// `RUN` without `CAP_NET_ADMIN`, under `--allow network.none-userns`
/// (ADR-0033): it creates a user namespace and a network namespace owned by
/// it, where it has `CAP_NET_ADMIN`, brings up `lo`, sends [`READY`] with a
/// descriptor of the namespace (`/proc/self/ns/net`) on `chan`, a
/// `SOCK_SEQPACKET` socket, and exits: the descriptor keeps the namespace
/// alive. The `RUN` child joins it with `setns` and stays in the build's
/// user namespace, under the filter of [`crate::seccomp`]. With `userns`
/// false it creates the network namespace alone: the probe that tells
/// whether the clone can do it itself.
///
/// # Safety
///
/// Call only in the child of a `clone` without `CLONE_VM`, right after it
/// returned 0: everything here is a plain syscall, with no allocation.
pub(crate) unsafe fn create_network(chan: RawFd, parent: i32, userns: bool) -> ! {
    check(
        chan,
        Step::DeathSignal,
        rustix::process::set_parent_process_death_signal(Some(Signal::KILL)),
    );
    // Not in a new PID namespace: `getppid` is the cloning process, unless it
    // died before the signal was armed.
    if rustix::process::getppid().map_or(0, |p| p.as_raw_nonzero().get()) != parent {
        fail(chan, Step::ParentGone, Errno::SRCH);
    }
    // A forked copy of every descriptor of the build, stdio included:
    // closed, so that the helper holds no lock, pipe end or terminal of it.
    close_all_but(chan);
    let mut flags = libc::CLONE_NEWNET;
    if userns {
        flags |= libc::CLONE_NEWUSER;
    }
    // SAFETY: plain syscall; the process is single-threaded (a clone).
    if unsafe { libc::unshare(flags) } != 0 {
        fail(chan, Step::NetNamespace, last_errno());
    }
    loopback_up(chan);
    let ns = check(
        chan,
        Step::NetNamespace,
        rustix::fs::openat(
            CWD,
            c"/proc/self/ns/net",
            OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
        ),
    );
    send_ready(chan, Some(ns.as_raw_fd()));
    // SAFETY: `_exit` without handlers, as in `fail`.
    unsafe { libc::_exit(0) }
}

/// Joins the network namespace `ns` (from [`create_network`]), then exits:
/// the probe's check that a child can, as the `RUN` child will need.
///
/// # Safety
///
/// As [`create_network`].
pub(crate) unsafe fn try_join(chan: RawFd, ns: RawFd) -> ! {
    // SAFETY: `setns` on a namespace descriptor the parent keeps open
    // across the clone.
    if unsafe { libc::setns(ns, libc::CLONE_NEWNET) } != 0 {
        fail(chan, Step::JoinNetwork, last_errno());
    }
    send_ready(chan, None);
    // SAFETY: `_exit` without handlers, as in `fail`.
    unsafe { libc::_exit(0) }
}

/// Sends [`READY`] on `chan`, with `fd` as `SCM_RIGHTS` when given, in one
/// message; buffers on the stack.
fn send_ready(chan: RawFd, fd: Option<RawFd>) {
    let mut data = [READY];
    // Room for one `cmsghdr` and one descriptor, aligned for `cmsghdr`.
    let mut control = [0u64; 4];
    // SAFETY: `msghdr` and `iovec` are plain integers and pointers; all-zero
    // is valid. The pointers refer to `data` and `control`, which live
    // across `sendmsg`; the `CMSG_*` helpers only compute addresses inside
    // `control`, which has room for one descriptor.
    let sent = unsafe {
        let mut iov = libc::iovec {
            iov_base: data.as_mut_ptr().cast(),
            iov_len: data.len(),
        };
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &raw mut iov;
        msg.msg_iovlen = 1;
        if let Some(fd) = fd {
            let int = u32::try_from(size_of::<libc::c_int>()).unwrap_or(4);
            msg.msg_control = control.as_mut_ptr().cast();
            msg.msg_controllen = libc::CMSG_SPACE(int) as _;
            let cmsg = libc::CMSG_FIRSTHDR(&raw const msg);
            if cmsg.is_null() {
                fail(chan, Step::NetNamespace, Errno::INVAL);
            }
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(int) as _;
            libc::CMSG_DATA(cmsg)
                .cast::<libc::c_int>()
                .write_unaligned(fd);
        }
        libc::sendmsg(chan, &raw const msg, libc::MSG_NOSIGNAL)
    };
    if sent != 1 {
        fail(chan, Step::NetNamespace, last_errno());
    }
}

/// Closes every descriptor except `keep`, from 0, as [`close_all_except`].
fn close_all_but(keep: RawFd) {
    close_all_except(&[keep]);
}

/// Closes, right after the clone, every descriptor the child copied from
/// ranikust but those it uses: stdio, the error pipe, the parent check's
/// `stat` file, the network namespace to join, the stdio sources and a
/// `#!` script. ranikust holds directories outside the root (the state
/// directory, the build context, stage roots); left open until `execve`,
/// `/proc/self/fd/<n>` would lead a `WORKDIR`, a `PATH` entry or a mount
/// target to them (runc's CVE-2024-21626). None of those kept is a
/// directory. Close-on-exec acts only at `execve`, after the child has
/// looked up paths the `RUN` chooses.
pub(crate) fn close_inherited(args: &ChildArgs, fds: ChildFds, parent: ParentCheck) {
    let [s0, s1, s2] = args.stdio.map(|s| match s {
        ChildStdio::Fd(fd) => fd,
        ChildStdio::Inherit | ChildStdio::Null => -1,
    });
    let mut keep: [RawFd; 10] = [
        0,
        1,
        2,
        fds.err_w,
        parent.stat,
        args.netns.unwrap_or(-1),
        s0,
        s1,
        s2,
        args.script.unwrap_or(-1),
    ];
    // Ascending, for `close_all_except`: ten entries, no allocation.
    for i in 1..keep.len() {
        let mut j = i;
        while j > 0 && keep[j - 1] > keep[j] {
            keep.swap(j - 1, j);
            j -= 1;
        }
    }
    close_all_except(&keep);
}

/// Closes every descriptor not in `keep`, which is sorted ascending and may
/// hold duplicates and negative entries (ignored): with `close_range` over
/// the gaps, else those `/proc/self/fd` lists, else every number up to the
/// hard limit.
fn close_all_except(keep: &[RawFd]) {
    let mut ok = true;
    let mut next: u32 = 0;
    for &k in keep {
        let Ok(k) = u32::try_from(k) else {
            continue;
        };
        if k > next {
            // SAFETY: plain syscall on a descriptor range.
            ok &= unsafe { libc::syscall(libc::SYS_close_range, next, k - 1, 0u32) } == 0;
        }
        next = next.max(k.saturating_add(1));
    }
    // SAFETY: as above.
    ok &= unsafe { libc::syscall(libc::SYS_close_range, next, u32::MAX, 0u32) } == 0;
    if ok || close_listed(keep) {
        return;
    }
    let limit = rustix::process::getrlimit(rustix::process::Resource::Nofile)
        .maximum
        .unwrap_or(1 << 20)
        .min(1 << 20);
    let limit = RawFd::try_from(limit).unwrap_or(1 << 20);
    for fd in 0..=limit {
        if !keep.contains(&fd) {
            // SAFETY: closing a descriptor number; a closed one fails with
            // EBADF.
            unsafe { libc::close(fd) };
        }
    }
}

/// Closes every descriptor `/proc/self/fd` lists except those in `keep`,
/// passing again until one pass finds none; `false` if the list cannot be
/// read.
fn close_listed(keep: &[RawFd]) -> bool {
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
                && !keep.contains(&fd)
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
pub(crate) fn drop_capabilities(err: RawFd, args: &ChildArgs) {
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
    trim_capabilities(err, args);
}

/// Clears the ambient and inheritable sets and keeps only Docker's defaults
/// in the permitted and effective sets: lowering them needs no privilege,
/// unlike the bounding set (ADR-0051). Same rules as
/// [`drop_capabilities`].
pub(crate) fn trim_capabilities(err: RawFd, args: &ChildArgs) {
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

/// Right after [`close_inherited`], before anything a `RUN` can steer is
/// looked up: the stdio sources the parent passed go on 0, 1 and 2, so
/// that ranikust's own streams are gone. `/dev/null` is never opened by
/// name here: a `RUN --mount` over `/dev`, or in `Direct` an earlier `RUN`,
/// could make it a link to `/proc/self/fd/1`, ranikust's stdout. A `Null`
/// slot (the full sandbox only; `Direct` gets the parent's descriptor)
/// holds a copy of the parent check's `stat` file until [`null_stdio`]
/// puts the sandbox's own `/dev/null` there, since a closed slot would let
/// a later open take its number. `Inherit` keeps ranikust's stream, as the
/// caller asked.
pub(crate) fn early_stdio(err: RawFd, stdio: &[ChildStdio; 3], parent: ParentCheck) {
    for (target, how) in (0..).zip(stdio) {
        match *how {
            ChildStdio::Inherit => {}
            ChildStdio::Fd(fd) => dup_to(err, fd, target),
            ChildStdio::Null => dup_to(err, parent.stat, target),
        }
    }
}

/// Puts `null`, the sandbox's `/dev/null` (a read-only bind of the outer
/// node, opened from the `/dev` tmpfs by descriptor), on every `Null` slot.
fn null_stdio(err: RawFd, stdio: &[ChildStdio; 3], null: &OwnedFd) {
    for (target, how) in (0..).zip(stdio) {
        if *how == ChildStdio::Null {
            dup_to(err, null.as_raw_fd(), target);
        }
    }
}

/// `null` in the `/dev` tmpfs `dev` just populated, read-write: the bind of
/// the outer `/dev/null`, a character device 1:3, on a read-only mount so
/// that a `RUN` cannot change the host's node through its stdin.
fn open_dev_null(err: RawFd, dev: BorrowedFd<'_>) -> OwnedFd {
    let null = check(
        err,
        Step::Stdio,
        rustix::fs::openat(
            dev,
            c"null",
            OFlags::RDWR | OFlags::NOFOLLOW | OFlags::NOCTTY | OFlags::CLOEXEC,
            Mode::empty(),
        ),
    );
    let st = check(err, Step::Stdio, rustix::fs::fstat(&null));
    let rdev = (rustix::fs::major(st.st_rdev), rustix::fs::minor(st.st_rdev));
    if FileType::from_raw_mode(st.st_mode) != FileType::CharacterDevice || rdev != (1, 3) {
        fail(err, Step::Stdio, Errno::NXIO);
    }
    null
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
    if let Some(fd) = args.script {
        exec_script(err, fd, ptrs);
    }
    let mut denied = false;
    for candidate in &args.candidates {
        // As `execve` resolves it, but through no magic link: before
        // `execve` this process is still a copy of ranikust, and
        // `/proc/self/exe` or a `/proc/self/fd/<n>` entry would lead out of
        // the root. Missing or refused, it is skipped as `execve` would.
        match lookup_no_magic_links(candidate) {
            Ok(()) => {}
            Err(Errno::NOENT | Errno::NOTDIR) => continue,
            Err(Errno::ACCESS) => {
                denied = true;
                continue;
            }
            Err(e) => fail(err, Step::Exec, e),
        }
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

/// Opens `path` as `execve` would find it, from the root and the working
/// directory, with `O_PATH` and `RESOLVE_NO_MAGICLINKS`, and closes it:
/// `ELOOP` for a path through a magic link.
fn lookup_no_magic_links(path: &CStr) -> rustix::io::Result<()> {
    let mut attempts = 0;
    loop {
        match rustix::fs::openat2(
            CWD,
            path,
            OFlags::PATH | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::NO_MAGICLINKS,
        ) {
            Ok(fd) => {
                drop(fd);
                return Ok(());
            }
            Err(Errno::AGAIN) if attempts < 16 => attempts += 1,
            Err(e) => return Err(e),
        }
    }
}

/// Executes the `#!` script held by the memfd `fd` (ADR-0057). The
/// interpreter opens the script again as `/dev/fd/<fd>`, which the kernel
/// refuses for a close-on-exec descriptor: `fd` alone leaves the
/// close-on-exec set [`reset_process_state`] put on every descriptor.
fn exec_script(err: RawFd, fd: RawFd, ptrs: &ExecPtrs<'_>) -> ! {
    // SAFETY: `F_SETFD` on a descriptor the parent kept open for the child.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, 0) } != 0 {
        fail(err, Step::Exec, last_errno());
    }
    // SAFETY: the path is an empty NUL-terminated string, `argv` and `envp`
    // are NULL-terminated arrays of pointers into a live `ChildArgs`;
    // `execveat` returns only on failure.
    unsafe {
        libc::syscall(
            libc::SYS_execveat,
            fd,
            c"".as_ptr(),
            ptrs.argv.as_ptr(),
            ptrs.envp.as_ptr(),
            libc::AT_EMPTY_PATH,
        );
    }
    fail(err, Step::Exec, last_errno())
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

    /// Every read-only and masked path is found under `proc` or `sysfs`,
    /// by the rest of its path.
    #[test]
    fn kernel_paths_split_into_filesystem_and_rest() {
        assert_eq!(kernel_path(c"/proc/sys"), Some((true, c"sys")));
        assert_eq!(
            kernel_path(c"/sys/devices/virtual/powercap"),
            Some((false, c"devices/virtual/powercap"))
        );
        assert_eq!(kernel_path(c"/proc/"), None);
        assert_eq!(kernel_path(c"/etc/passwd"), None);
        assert_eq!(kernel_path(c"/procx/sys"), None);
        for p in READONLY_PATHS.into_iter().chain(MASKED_PATHS) {
            let (_, rest) = kernel_path(p).unwrap();
            assert!(!rest.to_bytes().starts_with(b"/"), "{p:?}");
        }
    }

    /// The record of binds made by path holds what fits and says so.
    #[test]
    fn made_mounts_record_ids_up_to_their_room() {
        let made = MadeMounts::with_room(2);
        assert!(!made.contains(7));
        assert!(made.push(7));
        assert!(made.contains(7));
        assert!(!made.contains(0), "unused room is not an id");
        assert!(made.push(9));
        assert!(!made.push(11), "full");
        assert!(made.contains(9) && !made.contains(11));
        let none = MadeMounts::default();
        assert!(!none.push(1));
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
            root_nodev_rec: false,
            mount_api: false,
            made: MadeMounts::default(),
            mounts: Vec::new(),
            secrets: None,
            dev_remount: [MountFlags::BIND; DEV_NODES.len()],
            dev_pipes: false,
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
            chroot: None,
            root_check: None,
            direct_drop: false,
            nofile: None,
            core: None,
            stdio: [ChildStdio::Inherit; 3],
            argv: vec![c"sh".into(), c"-c".into()],
            envp: vec![c"A=1".into()],
            candidates: Vec::new(),
            script: None,
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
