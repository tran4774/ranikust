//! The parent side of a `RUN`: preparing the child's arguments and stubs,
//! the spawner thread that clones every child, reading the failure record,
//! reaping with `wait4` and its resource usage.

use std::ffi::{CString, OsStr};
use std::fs::{self, File, Permissions};
use std::io::{self, Write as _};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{
    Mutex, MutexGuard, OnceLock, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard, mpsc,
};
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

use rnk_rootfs::{RootPath, User, chown_id};
use rustix::fs::{FileType, Mode, OFlags};
use rustix::io::Errno;
use rustix::mount::MountFlags;
use rustix::pipe::PipeFlags;
use rustix::process::{Gid, Pid, Signal, Uid, WaitId, WaitIdOptions};
use tracing::Span;
use tracing::field::Empty;

use crate::caps;
use crate::cgroup::{self, Killer, RunCgroup};
use crate::child::{
    self, ChildArgs, ChildFds, ChildMount, ChildSecret, ChildSecrets, ChildStdio, ExecPtrs,
    KeyringJoin, MadeMounts, RootEntry,
};
use crate::direct;
use crate::error::{Error, Result, Step, io_err, sys_err};
use crate::fd::fd_above_stdio;
use crate::journal::{self, Content, Journal};
use crate::plan::{self, DEV_NODES, EtcFiles, MountOp};
use crate::probe::{Caps, KernelFs};
use crate::relay::Relay;
use crate::seccomp::{self, Program, SeccompProfile};
use crate::spec::{
    DEFAULT_HOSTNAME, Isolation, MountKind, Network, RootId, RunSpec, SecretFile, Stdio, root_id,
};
use crate::sweep;
use crate::userns::OwnNamespace;

/// Runs `RUN` commands.
///
/// Owns the spawner thread every child is cloned from: `PR_SET_PDEATHSIG`
/// fires when the parent *thread* exits, so that thread lives as long as the
/// `Sandbox`, and dropping the `Sandbox` kills every `RUN` still running.
#[derive(Debug)]
pub struct Sandbox {
    runner: Runner,
    running: Running,
    requests: Option<mpsc::Sender<Request>>,
    thread: Option<JoinHandle<()>>,
    // `TURNS`, or in a test a lock of its own, so a failed test breaks no
    // other test's turns.
    turns: &'static Turns,
}

impl Sandbox {
    /// Replays the stub journals that dead builds left in `dir/runs`, then
    /// starts the spawner thread. `dir` is the sandbox's directory in the
    /// state directory, `<state-dir>/sandbox`; `caps` is what [`crate::probe`]
    /// found. Such a sandbox runs [`Isolation::Full`] on any root, and
    /// [`Isolation::Direct`] on `/` only: a bare `chroot` is never used where
    /// the full sandbox can run (ADR-0014, ADR-0060). Where [`Caps::proc`]
    /// is [`KernelFs::Rbind`] it runs no `Direct` `RUN` at all: a turn ends
    /// with the `RUN`'s PID namespace, which `Direct` lacks (ADR-0127).
    /// Kaniko mode that cannot mount uses [`Sandbox::start_direct`]. Every
    /// `Full` `RUN` runs under [`SeccompProfile::docker_default`] until
    /// [`Sandbox::set_seccomp`] says otherwise (ADR-0103).
    ///
    /// Where ranikust's cgroup v2 tree is delegated, it also probes the
    /// cgroups each [`Isolation::Full`] `RUN` gets there, and may move this
    /// process into a cgroup of its own until the `Sandbox` is dropped
    /// (ADR-0110); elsewhere, or when that probe fails, nothing changes.
    ///
    /// # Errors
    ///
    /// The directory cannot be created or is a symlink, a journal cannot be
    /// listed, the default seccomp profile cannot be compiled here
    /// ([`Error::SeccompProfile`]), or the thread cannot start.
    pub fn start(caps: Caps, dir: &Path) -> Result<Self> {
        Self::start_inner(Some(caps), dir, None)
    }

    /// [`Sandbox::start`] for kaniko mode on a shared root outside the state
    /// directory (a host `--workdir`, already checked and locked by the
    /// caller): journals of [`Isolation::Direct`] `RUN`s on exactly that
    /// root are replayed too, which [`Sandbox::start`] refuses for any root
    /// outside the state directory. The root is opened once, without
    /// following a symlink, and refused when it is the host's `/`, `/etc` or
    /// `/dev` by device and inode (a bind mount of `/` included): nothing is
    /// placed there. Journals are matched to it by identity, never by path.
    /// `checked` is the root as the caller opened and checked it: the path
    /// must still lead to that directory (device, inode, mount id), and
    /// every later `RUN` must be on that path and that directory.
    ///
    /// # Errors
    ///
    /// As [`Sandbox::start`], and [`Error::InvalidSpec`] when `shared_root`
    /// is not absolute, is the host's `/`, `/etc` or `/dev`, or is no longer
    /// the directory `checked` is.
    ///
    /// Test-only: with `caps` `None` it turns on the bare-`chroot` `Direct`
    /// path off `/`, which the binary never uses (ADR-0050, ADR-0060); it is
    /// kept out of the library's API so that no caller turns it on by
    /// mistake.
    #[cfg(test)]
    pub(crate) fn start_with_root(
        caps: Option<Caps>,
        dir: &Path,
        shared_root: &Path,
        checked: &RootPath,
    ) -> Result<Self> {
        if !shared_root.is_absolute() {
            return Err(Error::InvalidSpec(format!(
                "the shared root must be absolute: {shared_root:?}"
            )));
        }
        let id = root_id(checked)?;
        let root = RootPath::open(shared_root)?.stay_on_mount();
        if root_id(&root)? != id {
            return Err(Error::InvalidSpec(format!(
                "the shared root {} is not the directory the caller checked",
                shared_root.display()
            )));
        }
        journal::refuse_host_system_dir(&root, shared_root)?;
        let mut sandbox = Self::start_inner(caps, dir, Some(&root))?;
        sandbox.runner.shared_root = Some((shared_root.to_owned(), id));
        Ok(sandbox)
    }

    /// Starts the spawner for kaniko mode on `/` in a container (ADR-0051):
    /// [`Isolation::Direct`] on `/` only, with no directory of its own (it
    /// places nothing, so it journals nothing), and the process made a
    /// child subreaper, so that whatever a `RUN` leaves behind, `setsid`
    /// ones included, becomes ranikust's child and is killed and reaped
    /// once the `RUN` ends. Process-wide; call it once, from the binary.
    ///
    /// # Errors
    ///
    /// [`Error::RunReachesState`] where the container grants capabilities
    /// beyond Docker's defaults and ranikust lacks `CAP_SETPCAP` to keep
    /// them from a `RUN`; `prctl(PR_SET_CHILD_SUBREAPER)` failed, or the
    /// thread cannot start.
    pub fn start_direct() -> Result<Self> {
        // Checked again for each `RUN`; here so that a container granting
        // capabilities ranikust cannot take from a `RUN` is refused before
        // the build pulls anything.
        direct_slash_drop()?;
        rustix::process::set_child_subreaper(Some(rustix::process::getpid()))
            .map_err(|e| sys_err("prctl(PR_SET_CHILD_SUBREAPER)", e))?;
        sweep::warn_ptrace_scope();
        Self::spawn_thread(Runner::direct_only())
    }

    fn start_inner(caps: Option<Caps>, dir: &Path, shared_root: Option<&RootPath>) -> Result<Self> {
        let mut runner = Runner::new(dir, caps.map(|c| c.root_entry))?;
        if let Some(c) = caps {
            runner = runner.with_kernel_fs(c.proc, c.sysfs);
        }
        let replayed = journal::replay(&runner.runs_dir, &runner.state_dir, shared_root)?;
        if replayed.journals > 0 {
            tracing::info!(
                "rnk-sandbox: removed {} stub(s) left by {} interrupted RUN(s)",
                replayed.stubs,
                replayed.journals
            );
        }
        // After the replay, which no profile error may hold up.
        runner.seccomp = Some(SeccompProfile::docker_default()?);
        // Only where the full sandbox runs (ADR-0110); after the profile,
        // whose error then leaves no cgroup to undo, and a later failure
        // drops the tree, which unnests it.
        if caps.is_some() {
            runner.cgroups = cgroup::Tree::start();
        }
        Self::spawn_thread(runner)
    }

    fn spawn_thread(runner: Runner) -> Result<Self> {
        let parent = ParentPid::new()?;
        let (requests, received) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("rnk-sandbox-spawner".into())
            .spawn(move || spawner(&received, parent))
            .map_err(|e| sys_err("start the spawner thread", e))?;
        Ok(Self {
            runner,
            running: Running::default(),
            requests: Some(requests),
            thread: Some(thread),
            turns: &TURNS,
        })
    }

    /// Runs one `RUN` and waits for it; the exit status is the command's.
    ///
    /// Blocking: an async caller runs it on a blocking thread. The child is
    /// cloned on the spawner thread whatever thread calls this. Where
    /// [`Caps::proc`] is [`KernelFs::Rbind`], a `RUN` runs alone among those
    /// of every `Sandbox` of the process: a call waits until none of them
    /// runs, and holds its turn until its `RUN` is reaped, the processes of
    /// its PID namespace gone with it. Other `RUN`s run side by side, but
    /// never during such a turn. Such a `Sandbox` refuses
    /// [`Isolation::Direct`], which has no PID namespace, before any wait
    /// (ADR-0127).
    ///
    /// # Errors
    ///
    /// [`Error::InvalidSpec`], [`Error::Rootfs`] for the `USER` lookup or a
    /// stub, [`Error::Setup`] with the failing step and errno, [`Error::Exec`]
    /// when the command cannot be executed, [`Error::Privilege`] for
    /// [`Isolation::Full`] without `CAP_SYS_ADMIN` or without [`Caps`],
    /// [`Error::Cancelled`] after [`Sandbox::cancel`],
    /// [`Error::TurnPoisoned`] once a `RUN` panicked or lost the spawner
    /// thread during its turn, or a host-side error.
    pub fn run(&self, spec: RunSpec) -> Result<ExitStatus> {
        let requests = self.requests.as_ref().ok_or(Error::SpawnerGone)?;
        let turn = self.turns.take(self.runner.kernel_fs.0, spec.isolation)?;
        self.runner.run(spec, Some(&self.running), |job| {
            let (reply, answer) = mpsc::sync_channel(1);
            requests
                .send(Request { job, reply })
                .map_err(|_| Error::SpawnerGone)?;
            answer.recv().map_err(|_| {
                // The spawner may have gone after the clone, its child alive.
                turn.break_turns();
                Error::SpawnerGone
            })?
        })
    }

    /// Allows `--network=none` through a user namespace where the process
    /// lacks `CAP_NET_ADMIN` (`--allow network.none-userns`, ADR-0033):
    /// such `RUN`s join a network namespace a user namespace owns and run
    /// under a seccomp filter. Without it they are refused. Call
    /// before the first `RUN`.
    pub fn allow_userns_network(&mut self) {
        self.runner.userns_network = true;
    }

    /// The seccomp profile of every later [`Isolation::Full`] `RUN`:
    /// `None` is `--seccomp=unconfined`, which leaves ADR-0033's filter of
    /// a `--network=none` `RUN` in a user namespace in place. A
    /// [`Isolation::Direct`] `RUN` never gets one (ADR-0103). Call before
    /// the first `RUN`.
    pub fn set_seccomp(&mut self, profile: Option<SeccompProfile>) {
        self.runner.seccomp = profile;
    }

    /// Every later `RUN`, of any isolation, starts with `limit` as its
    /// `RLIMIT_NOFILE`: the limit ranikust started with, captured with
    /// [`NofileLimit::current`](crate::NofileLimit::current) before it
    /// raised its own for the kaniko-mode walk. Without it a `RUN` inherits
    /// ranikust's limit. Call before the first `RUN`.
    pub fn set_nofile_limit(&mut self, limit: crate::NofileLimit) {
        self.runner.nofile = Some(limit);
    }

    /// Cancels the build's `RUN`s: kills every one running with `SIGKILL`,
    /// as BuildKit does on a cancel, through its `cgroup.kill` too where it
    /// has a cgroup, and makes every later [`Sandbox::run`] fail with
    /// [`Error::Cancelled`]. Stubs are cleaned up as after any `RUN`, before
    /// `run` returns. Any thread may call it.
    pub fn cancel(&self) {
        self.running.cancel();
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // The thread ends when the channel closes.
        drop(self.requests.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        // No `RUN` is left: the nesting is undone, ranikust back in its
        // cgroup (ADR-0110).
        drop(self.runner.cgroups.take());
    }
}

/// Makes `d` exist as a directory that is not a symlink: an existing entry
/// is checked before anything is created through it, and a missing one is
/// created with `mkdir`, which does not follow a symlink in its place
/// (with its ancestors when `ancestors`).
fn real_dir(d: &Path, ancestors: bool) -> Result<()> {
    let not_a_dir = || {
        io_err(
            "use as the sandbox directory",
            d,
            io::Error::new(io::ErrorKind::InvalidInput, "not a directory (a symlink?)"),
        )
    };
    match fs::symlink_metadata(d) {
        Ok(st) if st.is_dir() => return Ok(()),
        Ok(_) => return Err(not_a_dir()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_err("stat", d, e)),
    }
    let created = if ancestors {
        fs::create_dir_all(d)
    } else {
        fs::create_dir(d)
    };
    created.map_err(|e| io_err("create", d, e))?;
    match fs::symlink_metadata(d) {
        Ok(st) if st.is_dir() => Ok(()),
        Ok(_) => Err(not_a_dir()),
        Err(e) => Err(io_err("stat", d, e)),
    }
}

/// A prepared child, to be cloned.
pub(crate) struct Job {
    args: ChildArgs,
    /// `clone` flags, the exit signal included.
    flags: libc::c_long,
    isolation: Isolation,
    /// Join a network namespace [`NetNs::create`] makes in its own user
    /// namespace, instead of cloning one (ADR-0033).
    hold_network: bool,
    /// Descriptors the child uses (the caller's stdio, mount sources), open
    /// until the clone and closed right after, so that only the child holds
    /// them.
    fds: Vec<OwnedFd>,
    /// This process runs in a user namespace other than the initial one
    /// ([`OwnNamespace::user_ns`]).
    user_ns: bool,
}

struct Request {
    job: Job,
    reply: mpsc::SyncSender<Result<Spawned>>,
}

/// A child cloned and not yet reaped.
pub(crate) struct Spawned {
    pid: Pid,
    /// Read end of the error pipe: end-of-file once `execve` succeeded.
    failures: OwnedFd,
    /// The full sandbox: read end of the status pipe, on which the `RUN`'s
    /// init writes the command's wait status before it exits (ADR-0080).
    status: Option<OwnedFd>,
    /// The network namespace the child joined, closed once it is reaped.
    netns: Option<NetNs>,
}

/// This process's pid as `/proc` numbers it: the parent pid its children
/// must see in their outer `/proc/self/stat` (`child::ParentCheck`). Read
/// from the same procfs, so it matches even when `/proc` belongs to an
/// outer PID namespace.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ParentPid(i32);

impl ParentPid {
    pub(crate) fn new() -> Result<Self> {
        let p = Path::new("/proc/self/stat");
        let stat = fs::read(p).map_err(|e| io_err("read", p, e))?;
        child::stat_pid_and_ppid(&stat)
            .map(|(pid, _)| Self(pid))
            .ok_or_else(|| io_err("parse", p, io::Error::from(io::ErrorKind::InvalidData)))
    }
}

/// [`fd_above_stdio`], as the crate's error.
fn above_stdio(fd: OwnedFd) -> Result<OwnedFd> {
    fd_above_stdio(fd).map_err(|e| sys_err("fcntl(F_DUPFD_CLOEXEC)", e))
}

fn spawner(requests: &mpsc::Receiver<Request>, parent: ParentPid) {
    for Request { job, reply } in requests {
        let spawned = clone_child(job, parent);
        // The caller waits on the answer; it can only be gone if it
        // panicked, which aborts the process.
        let _ = reply.send(spawned);
    }
}

/// Clones `job` on the calling thread, which becomes the child's parent
/// thread for `PR_SET_PDEATHSIG`.
pub(crate) fn clone_child(job: Job, parent: ParentPid) -> Result<Spawned> {
    let Job {
        mut args,
        flags,
        isolation,
        hold_network,
        fds: keep,
        user_ns: _,
    } = job;
    let (failures, err_w) =
        rustix::pipe::pipe_with(PipeFlags::CLOEXEC).map_err(|e| sys_err("pipe2", e))?;
    let err_w = above_stdio(err_w)?;
    // The full sandbox's init reports the command's wait status here
    // (ADR-0080); `Direct` has no init.
    let status = match isolation {
        Isolation::Full => {
            let (r, w) =
                rustix::pipe::pipe_with(PipeFlags::CLOEXEC).map_err(|e| sys_err("pipe2", e))?;
            Some((r, above_stdio(w)?))
        }
        Isolation::Direct => None,
    };
    // Last before the clone: nothing can fail between it and the clone
    // without ending it.
    let netns = if hold_network {
        let ns =
            NetNs::create(true).map_err(|e| sys_err("create the RUN's network namespace", e))?;
        args.netns = Some(ns.fd.as_raw_fd());
        Some(ns)
    } else {
        None
    };
    let ptrs = ExecPtrs::new(&args);
    let fds = ChildFds {
        err_w: err_w.as_raw_fd(),
        parent: parent.0,
        status_w: status.as_ref().map_or(-1, |(_, w)| w.as_raw_fd()),
    };
    let null: libc::c_long = 0;
    // SAFETY: `clone` without `CLONE_VM` and with a null stack duplicates the
    // process as `fork` does: the child continues here on a copy of this
    // thread's stack and memory. The arguments are integers.
    let ret = unsafe { libc::syscall(libc::SYS_clone, flags, null, null, null, null) };
    if ret == 0 {
        // SAFETY: this is the child of that clone, right after it returned
        // 0; `args`, `ptrs` and `fds` were built before it. Neither function
        // returns.
        unsafe {
            match isolation {
                Isolation::Direct => direct::run(&args, &ptrs, fds),
                Isolation::Full => child::run(&args, &ptrs, fds),
            }
        }
    }
    // Closed before the next clone, so that only this child holds them.
    drop(keep);
    if ret < 0 {
        let e = io::Error::last_os_error();
        drop(netns);
        return Err(match e.raw_os_error() {
            Some(libc::EPERM) => Error::Privilege("creating the RUN namespaces"),
            _ => sys_err("clone", e),
        });
    }
    let pid = i32::try_from(ret)
        .ok()
        .and_then(Pid::from_raw)
        .ok_or_else(|| sys_err("clone", io::Error::from(io::ErrorKind::InvalidData)))?;
    drop(err_w);
    Ok(Spawned {
        pid,
        failures,
        status: status.map(|(r, _w)| r),
        netns,
    })
}

/// A network namespace for `--network=none` without `CAP_NET_ADMIN`
/// (ADR-0033): a descriptor of it, which keeps it alive, made by a helper
/// ([`child::create_network`]) that has exited. The `RUN` child joins it
/// with `setns`.
#[derive(Debug)]
pub(crate) struct NetNs {
    fd: OwnedFd,
}

/// `NSFS_MAGIC`: the file system of `/proc/<pid>/ns/*`.
const NSFS_MAGIC: i64 = 0x6e73_6673;

impl NetNs {
    /// A new network namespace with `lo` up: in a new user namespace with
    /// `userns`, else in the caller's (the probe).
    fn create(userns: bool) -> io::Result<Self> {
        let me = rustix::process::getpid().as_raw_nonzero().get();
        let fd = run_helper(|chan| {
            // SAFETY: the child of the helper's clone, right after it.
            unsafe { child::create_network(chan, me, userns) }
        })?
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
        // A namespace descriptor, not something else the helper opened.
        let st = rustix::fs::fstatfs(&fd)?;
        #[allow(
            clippy::useless_conversion,
            reason = "f_type's integer type differs between targets"
        )]
        let fs_type = i64::from(st.f_type);
        if fs_type != NSFS_MAGIC {
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }
        // A new namespace, not this process's own.
        let own = rustix::fs::open(
            "/proc/self/ns/net",
            OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let (new, mine) = (rustix::fs::fstat(&fd)?, rustix::fs::fstat(&own)?);
        if (new.st_dev, new.st_ino) == (mine.st_dev, mine.st_ino) {
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }
        Ok(Self { fd })
    }

    /// Whether a child can join this namespace, as the `RUN` child will: a
    /// helper that joins it and exits.
    fn check_join(&self) -> io::Result<()> {
        let ns = self.fd.as_raw_fd();
        run_helper(|chan| {
            // SAFETY: the child of the helper's clone, right after it.
            unsafe { child::try_join(chan, ns) }
        })
        .map(drop)
    }
}

/// Clones a helper running `body` (which never returns) with its end of a
/// close-on-exec `SOCK_SEQPACKET` pair, waits for its one message, then
/// kills (it has nothing left to do) and reaps it. The message is
/// [`child::READY`], with the descriptor it sent if any, or an 8-byte
/// failure record, whose errno is returned; end-of-file (a helper killed
/// early) is an error, never readiness.
fn run_helper(body: impl FnOnce(RawFd)) -> io::Result<Option<OwnedFd>> {
    let mut pair = [-1; 2];
    // SAFETY: `socketpair` writes two descriptors into `pair`, owned below.
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            pair.as_mut_ptr(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both are new descriptors this function owns.
    let (ours, theirs) = unsafe { (OwnedFd::from_raw_fd(pair[0]), OwnedFd::from_raw_fd(pair[1])) };
    let theirs = above_stdio(theirs).map_err(|_| io::Error::from_raw_os_error(libc::EMFILE))?;
    let null: libc::c_long = 0;
    // SAFETY: as in `clone_child`: a `fork`-like clone; the child only makes
    // plain syscalls and never returns from `body`.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_clone,
            libc::c_long::from(libc::SIGCHLD),
            null,
            null,
            null,
            null,
        )
    };
    if ret == 0 {
        body(theirs.as_raw_fd());
        // SAFETY: `body` does not return; `_exit` if it ever did.
        unsafe { libc::_exit(127) }
    }
    drop(theirs);
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    let pid = i32::try_from(ret)
        .ok()
        .and_then(Pid::from_raw)
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
    let answer = receive(&ours);
    // Not reaped yet: the pid is the helper's. Done or failed, it has
    // nothing left to do.
    let _ = rustix::process::kill_process(pid, Signal::KILL);
    reap_helper(pid);
    answer
}

/// Receives a helper's message on `chan`: see [`run_helper`].
fn receive(chan: &OwnedFd) -> io::Result<Option<OwnedFd>> {
    let mut data = [0u8; 8];
    let mut control = [0u64; 4];
    // SAFETY: `msghdr` and `iovec` are plain integers and pointers; all-zero
    // is valid. The pointers refer to `data` and `control`, which live
    // across `recvmsg`; the `CMSG_*` helpers walk only `control`.
    unsafe {
        let mut iov = libc::iovec {
            iov_base: data.as_mut_ptr().cast(),
            iov_len: data.len(),
        };
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &raw mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = size_of_val(&control) as _;
        let n = loop {
            let n = libc::recvmsg(chan.as_raw_fd(), &raw mut msg, libc::MSG_CMSG_CLOEXEC);
            if n >= 0 || io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                break n;
            }
        };
        // Every descriptor received, owned at once so that none leaks.
        let mut fds = Vec::new();
        let mut cmsg = libc::CMSG_FIRSTHDR(&raw const msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(cmsg).cast::<libc::c_int>();
                let len = (*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                for i in 0..len / size_of::<libc::c_int>() {
                    fds.push(OwnedFd::from_raw_fd(data.add(i).read_unaligned()));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&raw const msg, cmsg);
        }
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if msg.msg_flags & libc::MSG_CTRUNC != 0 || fds.len() > 1 {
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }
        match (n, data) {
            (1, [child::READY, ..]) => Ok(fds.pop()),
            (8, record) => match child::decode_failure(record) {
                Some((_, _, errno)) => Err(io::Error::from_raw_os_error(errno)),
                None => Err(io::Error::from(io::ErrorKind::InvalidData)),
            },
            _ => Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
        }
    }
}

/// Reaps a helper process.
fn reap_helper(pid: Pid) {
    loop {
        match rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::empty()) {
            Err(Errno::INTR) => {}
            // Reaped, or not a child any more: nothing else to do.
            _ => return,
        }
    }
}

/// How a `--network=none` `RUN` gets a network namespace with loopback up,
/// found once by [`probe_network`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NetMode {
    /// The clone creates it and the child brings up `lo`: the process has
    /// `CAP_NET_ADMIN`.
    Clone,
    /// [`NetNs::create`] makes it in a user namespace of its own, and the
    /// child runs under [`crate::seccomp::userns_network`] (ADR-0033).
    UserNs,
    /// The process lacks `CAP_NET_ADMIN` and the user-namespace path is not
    /// allowed: the errno of the attempt without it.
    NotAllowed { direct: i32 },
    /// Allowed, but neither works: the errnos of the two attempts.
    Unavailable { direct: i32, userns: i32 },
}

/// Tries a network namespace with `lo` up without a user namespace, then,
/// when `userns` allows it, with one (and a child joining it), as the `RUN`
/// child would.
fn probe_network(userns: bool) -> NetMode {
    let direct = match NetNs::create(false) {
        Ok(_) => return NetMode::Clone,
        Err(e) => e.raw_os_error().unwrap_or(libc::EIO),
    };
    if !userns {
        return NetMode::NotAllowed { direct };
    }
    if seccomp::userns_network().is_none() {
        return NetMode::Unavailable {
            direct,
            userns: libc::ENOSYS,
        };
    }
    let held = NetNs::create(true).and_then(|ns| ns.check_join());
    match held {
        Ok(()) => NetMode::UserNs,
        Err(e) => NetMode::Unavailable {
            direct,
            userns: e.raw_os_error().unwrap_or(libc::EIO),
        },
    }
}

/// Prepares, runs and cleans up `RUN`s; the cloning is the caller's.
#[derive(Debug)]
pub(crate) struct Runner {
    runs_dir: PathBuf,
    /// The parent of the sandbox's directory: every root a journal may name
    /// lies below it.
    state_dir: PathBuf,
    /// `None`: the probe failed, only [`Isolation::Direct`] runs.
    root_entry: Option<RootEntry>,
    drops: caps::BoundingDrops,
    next: AtomicU64,
    /// Found at the first `--network=none` `RUN`.
    net_mode: OnceLock<NetMode>,
    /// `--allow network.none-userns`: without `CAP_NET_ADMIN`, the
    /// user-namespace path of [`NetMode::UserNs`] (ADR-0033).
    userns_network: bool,
    /// The `RLIMIT_NOFILE` every `RUN` starts with; `None` inherits.
    nofile: Option<crate::NofileLimit>,
    /// Kaniko mode's shared root given to [`Sandbox::start_with_root`] and
    /// its identity then (device, inode, mount id): a `RUN` on that path
    /// whose reopened root is another directory is refused.
    shared_root: Option<(PathBuf, RootId)>,
    /// [`Sandbox::start_direct`]: `Direct` on `/` only, which places nothing
    /// and so has no journal and no directory (ADR-0051).
    direct_only: bool,
    /// Whether a full-sandbox child may use `open_tree` and `move_mount`;
    /// `None` probes ([`mount_api_supported`]).
    mount_api: Option<bool>,
    /// The delegated cgroup tree each [`Isolation::Full`] `RUN` gets
    /// cgroups in (ADR-0110); `None` where there is none.
    cgroups: Option<cgroup::Tree>,
    /// The profile of a [`Isolation::Full`] `RUN`; `None` for the probe's
    /// runner, [`Sandbox::start_direct`]'s and `--seccomp=unconfined`.
    seccomp: Option<SeccompProfile>,
    /// The probe's runner ([`Runner::for_probe`]).
    probe: bool,
    /// How a full-sandbox `RUN` gets `proc` and `sysfs` ([`Caps`]).
    kernel_fs: (KernelFs, KernelFs),
    /// What the user namespace this process is in allows a `RUN`.
    own_ns: OwnNamespace,
}

/// Refuses a `root_dir` that is not the path the kernel gives its
/// directory (`/proc/thread-self/fd/<n>`): a symlink on the way, or a
/// directory since moved or removed. The root is compared with mount points
/// byte for byte ([`refuse_submounts`]); the binary always passes resolved
/// paths. The descriptor read is checked to be `root`'s directory.
fn refuse_unresolved(root_dir: &Path, root: &RootPath) -> Result<()> {
    let fd = rustix::fs::open(
        root_dir,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| io_err("open", root_dir, e.into()))?;
    let link = Path::new("/proc/thread-self/fd").join(fd.as_raw_fd().to_string());
    let resolved = fs::read_link(&link).map_err(|e| io_err("readlink", &link, e))?;
    let st = rustix::fs::statx(
        &fd,
        c"",
        rustix::fs::AtFlags::EMPTY_PATH,
        rustix::fs::StatxFlags::BASIC_STATS,
    )
    .map_err(|e| io_err("statx", root_dir, e.into()))?;
    let want = root.statx("/")?;
    let same = (st.stx_dev_major, st.stx_dev_minor, st.stx_ino)
        == (want.dev_major, want.dev_minor, want.ino);
    if resolved != root_dir || !same {
        return Err(Error::InvalidSpec(format!(
            "the root {} must be given resolved: the kernel names it {}",
            root_dir.display(),
            resolved.display()
        )));
    }
    Ok(())
}

impl Runner {
    /// A runner keeping its journals in `dir/runs`, created if missing.
    /// Neither `dir` nor `dir/runs` may be a symlink: a planted one would
    /// send the journals, and what a replay removes, elsewhere.
    pub(crate) fn new(dir: &Path, root_entry: Option<RootEntry>) -> Result<Self> {
        let runs_dir = dir.join("runs");
        real_dir(dir, true)?;
        real_dir(&runs_dir, false)?;
        Ok(Self {
            state_dir: dir.parent().unwrap_or(dir).to_path_buf(),
            runs_dir,
            root_entry,
            drops: caps::BoundingDrops::current(),
            next: AtomicU64::new(0),
            net_mode: OnceLock::new(),
            userns_network: false,
            nofile: None,
            shared_root: None,
            direct_only: false,
            mount_api: None,
            cgroups: None,
            seccomp: None,
            probe: false,
            kernel_fs: (KernelFs::Fresh, KernelFs::Fresh),
            own_ns: OwnNamespace::read(),
        })
    }

    /// A runner for `Direct` on `/` only: no directory, no journal, nothing
    /// to replay (ADR-0051).
    fn direct_only() -> Self {
        Self {
            runs_dir: PathBuf::new(),
            state_dir: PathBuf::new(),
            root_entry: None,
            drops: caps::BoundingDrops::current(),
            next: AtomicU64::new(0),
            net_mode: OnceLock::new(),
            userns_network: false,
            nofile: None,
            shared_root: None,
            direct_only: true,
            mount_api: None,
            cgroups: None,
            seccomp: None,
            probe: false,
            kernel_fs: (KernelFs::Fresh, KernelFs::Fresh),
            own_ns: OwnNamespace::read(),
        }
    }

    pub(crate) fn runs_dir(&self) -> &Path {
        &self.runs_dir
    }

    pub(crate) fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// The probe's runner: its command does not exist, so nothing it starts
    /// executes, and a refused keyring join is left to the `RUN`s
    /// that follow, under their own profile (ADR-0109).
    pub(crate) fn for_probe(mut self) -> Self {
        self.probe = true;
        self
    }

    /// Gives full-sandbox `RUN`s `proc` and `sysfs` as `proc` and `sysfs`
    /// say, as the probe found them ([`Caps`]).
    pub(crate) fn with_kernel_fs(mut self, proc: KernelFs, sysfs: KernelFs) -> Self {
        self.kernel_fs = (proc, sysfs);
        self
    }

    /// Makes `--network=none` use a [`NetNs`] whatever the probe would
    /// find: the path of an environment without `CAP_NET_ADMIN`, tested
    /// where the process has it.
    #[cfg(test)]
    pub(crate) fn hold_networks(&self) {
        let _ = self.net_mode.set(NetMode::UserNs);
    }

    /// Makes the child bind and remount as where `open_tree` and
    /// `move_mount` are refused, whatever the probe would find.
    #[cfg(test)]
    pub(crate) fn without_mount_api(&mut self) {
        self.mount_api = Some(false);
    }

    /// Gives full-sandbox `RUN`s cgroups in `tree`, as [`Sandbox::start`]
    /// does where one is delegated.
    #[cfg(test)]
    pub(crate) fn use_cgroups(&mut self, tree: cgroup::Tree) {
        self.cgroups = Some(tree);
    }

    /// Runs `spec`, cloning through `spawn`. With `running` (a
    /// [`Sandbox`]'s, not the probe's), the `RUN` can be cancelled and
    /// records the `rnk_sandbox.setup` and `rnk_sandbox.run` spans.
    pub(crate) fn run(
        &self,
        mut spec: RunSpec,
        running: Option<&Running>,
        spawn: impl FnOnce(Job) -> Result<Spawned>,
    ) -> Result<ExitStatus> {
        let traced = running.is_some();
        spec.validate()?;
        let root_entry = match (spec.isolation, self.root_entry) {
            // A turn under an rbind of `/proc` ends with the `RUN`'s PID
            // namespace, which `Direct` has none of (ADR-0127).
            (Isolation::Direct, _) if is_rbind(self.kernel_fs.0) => {
                return Err(Error::InvalidSpec(DIRECT_UNDER_RBIND.into()));
            }
            (Isolation::Full, None) => return Err(Error::Privilege("the RUN sandbox")),
            // A bare `chroot` is never used where mounting is possible: the
            // full sandbox runs that root (ADR-0014, ADR-0043).
            (Isolation::Direct, Some(_)) if spec.root_dir != Path::new("/") => {
                return Err(Error::InvalidSpec(
                    "Isolation::Direct on a root other than / where the full sandbox can \
                     run: use Isolation::Full"
                        .into(),
                ));
            }
            (Isolation::Full, Some(entry)) => entry,
            // Unused by `Direct`.
            (Isolation::Direct, entry) => entry.unwrap_or(RootEntry::PivotRoot),
        };
        let program = spec.argv[0].to_string_lossy().into_owned();
        let setup = if traced {
            tracing::debug_span!("rnk_sandbox.setup", stubs = Empty)
        } else {
            Span::none()
        };
        let (mut job, journal) = setup.in_scope(|| self.prepare(&mut spec, root_entry))?;
        setup.record("stubs", journal.as_ref().map_or(0, Journal::stub_count));
        // The full sandbox only: `Direct` on `/` keeps ranikust's cgroup
        // (ADR-0110).
        let cgroup = match (job.isolation, &self.cgroups) {
            (Isolation::Full, Some(tree)) => match setup.in_scope(|| tree.run_cgroup()) {
                Ok(c) => Some(c),
                Err(e) => {
                    abandon(journal, None);
                    return Err(e);
                }
            },
            (Isolation::Full, None) | (Isolation::Direct, _) => None,
        };
        if let Some(c) = &cgroup {
            job.args.cgroup_init = c.init_procs();
            job.args.cgroup_command = Some(c.command_procs());
        }
        let relay = match Relay::start(
            &mut job.args.stdio,
            &mut job.fds,
            job.args.uid,
            job.args.gid,
        ) {
            Ok(relay) => relay,
            Err(e) => {
                abandon(journal, cgroup);
                return Err(e);
            }
        };
        drop(setup);

        let span = if traced {
            tracing::debug_span!(
                "rnk_sandbox.run",
                user_ns = Empty,
                sys_ns = Empty,
                max_rss_kb = Empty,
                minflt = Empty,
                majflt = Empty,
                inblock = Empty,
                oublock = Empty,
                nvcsw = Empty,
                nivcsw = Empty,
                failed = Empty,
            )
        } else {
            Span::none()
        };
        let killer = cgroup.as_ref().map(RunCgroup::killer);
        let ran =
            span.in_scope(|| execute(job, spawn, &program, running, self.direct_only, killer));
        match &ran {
            Ok((status, usage)) => {
                record_usage(&span, usage);
                span.record("failed", !status.success());
            }
            Err(_) => {
                span.record("failed", true);
            }
        }
        drop(span);
        // Reaped: what is left of the `RUN` is killed and its cgroups
        // removed before the caller diffs the root.
        let removed = cgroup.map_or(Ok(()), RunCgroup::remove);
        let result = match (ran, removed) {
            (ran, Ok(())) => ran,
            (Ok(_), Err(e)) => Err(e),
            (Err(e), Err(c)) => {
                tracing::warn!("rnk-sandbox: {c}, after a failed RUN");
                Err(e)
            }
        };
        // The RUN is reaped and swept (or failed): the relay copies what is
        // left without waiting for a write end that outlived it.
        if let Some(relay) = relay {
            relay.finish();
        }

        // Stubs go before the caller diffs the root, whatever happened.
        let cleanup = journal.map_or(Ok(0), Journal::finish);
        match (result, cleanup) {
            (Ok((status, _)), Ok(_)) => Ok(status),
            (Ok(_), Err(e)) => Err(e),
            (Err(e), Ok(_)) => Err(e),
            (Err(e), Err(c)) => {
                tracing::warn!("rnk-sandbox: stub clean-up failed after a failed RUN: {c}");
                Err(e)
            }
        }
    }

    /// The child's arguments, with the stubs placed and journaled.
    fn prepare(&self, spec: &mut RunSpec, root_entry: RootEntry) -> Result<(Job, Option<Journal>)> {
        let direct = spec.isolation == Isolation::Direct;
        let root = RootPath::open(&spec.root_dir)?;
        refuse_unresolved(&spec.root_dir, &root)?;
        // The caller's root, by identity: the path is reopened here and
        // again in the child.
        if let Some(want) = spec.root_id
            && root_id(&root)? != want
        {
            return Err(Error::InvalidSpec(format!(
                "the root {} is no longer the directory the caller holds",
                spec.root_dir.display()
            )));
        }
        // The shared root is reopened by path for each `RUN`: it must still
        // be the directory checked at the start, for `Full` and `Direct`.
        if let Some((path, id)) = &self.shared_root {
            if *path != spec.root_dir {
                return Err(Error::InvalidSpec(format!(
                    "a RUN on {} while the build's shared root is {}",
                    spec.root_dir.display(),
                    path.display()
                )));
            }
            if root_id(&root)? != *id {
                return Err(Error::InvalidSpec(format!(
                    "the shared root {} is no longer the directory it was at the start",
                    path.display()
                )));
            }
        }
        let user = rnk_rootfs::lookup_user(&root, spec.user_spec())?;
        let stdio = [
            std::mem::take(&mut spec.stdin),
            std::mem::take(&mut spec.stdout),
            std::mem::take(&mut spec.stderr),
        ];
        if direct && spec.root_dir == Path::new("/") {
            // As kaniko: the container's own files, nothing placed; but no
            // capability that reads ranikust's state (ADR-0051).
            let drop = direct_slash_drop()?;
            let mut job = self.job(spec, &user, Vec::new(), None, stdio, root_entry, None)?;
            job.args.direct_drop = drop;
            if let Some(body) = &spec.script {
                let fd = script_memfd(body)?;
                job.args.script = Some(fd.as_raw_fd());
                job.fds.push(fd);
            }
            return Ok((job, None));
        }
        if direct && spec.script.is_some() {
            // The interpreter reads the script as `/dev/fd/<n>`, which a
            // `chroot` root without `/proc` lacks.
            return Err(Error::Direct(format!(
                "a #! heredoc runs from /dev/fd, which the root {} lacks",
                spec.root_dir.display()
            )));
        }
        if self.direct_only {
            return Err(Error::InvalidSpec(format!(
                "this sandbox runs Isolation::Direct on / only, not on {}",
                spec.root_dir.display()
            )));
        }
        let chroot = if direct {
            // By identity, not by name: a bind mount of `/` passes any
            // string check, and placing would replace the host's files.
            journal::refuse_host_system_dir(&root, &spec.root_dir)?;
            let st = root.statx("/")?;
            Some((st.dev_major, st.dev_minor, st.ino))
        } else {
            None
        };
        if !direct {
            refuse_symlinked_kernel_dirs(&root, &spec.root_dir)?;
        }
        let mut journal = Journal::create(&self.runs_dir, &self.next_id(), &spec.root_dir, root)?;
        if direct {
            return match place_direct(&mut journal)
                .and_then(|()| self.job(spec, &user, Vec::new(), None, stdio, root_entry, chroot))
            {
                Ok(job) => Ok((job, Some(journal))),
                Err(e) => {
                    if let Err(c) = journal.finish() {
                        tracing::warn!("rnk-sandbox: clean-up of placed files failed: {c}");
                    }
                    Err(e)
                }
            };
        }
        match place(&mut journal, spec).and_then(|files| {
            let mounts = plan::mount_plan(spec, &files, self.kernel_fs.0, self.kernel_fs.1);
            self.job(
                spec,
                &user,
                mounts,
                Some(journal.scratch()),
                stdio,
                root_entry,
                None,
            )
        }) {
            Ok(job) => Ok((job, Some(journal))),
            Err(e) => {
                if let Err(c) = journal.finish() {
                    tracing::warn!("rnk-sandbox: stub clean-up failed: {c}");
                }
                Err(e)
            }
        }
    }

    /// The child's arguments. `chroot`: a `Direct` root other than `/`, by
    /// the identity the child checks after entering it.
    #[expect(
        clippy::too_many_arguments,
        reason = "the spec and the parts the parent resolved for it"
    )]
    fn job(
        &self,
        spec: &RunSpec,
        user: &User,
        mounts: Vec<MountOp>,
        scratch: Option<&Path>,
        stdio: [Stdio; 3],
        root_entry: RootEntry,
        chroot: Option<(u32, u32, u64)>,
    ) -> Result<Job> {
        let env = plan::environment(&spec.env, &user.home);
        let candidates = plan::exec_candidates(&spec.argv[0], plan::path_var(&env));
        let mut keep = Vec::new();
        // `Direct`: the root's `/dev/null` is the image's entry (missing
        // where `mknod` is refused), or on `/` one an earlier `RUN` may have
        // replaced, so the host's is opened here and checked instead. The
        // full sandbox's child opens the read-only bind of it in its own
        // `/dev`: a descriptor of the host's node would let a root `RUN`
        // change its mode through `/proc/self/fd/0`.
        let stdio = if spec.isolation == Isolation::Direct {
            stdio.map(|s| match s {
                Stdio::Null => host_null().map(Stdio::Fd),
                s => Ok(s),
            })
        } else {
            stdio.map(Ok)
        };
        let [stdin, stdout, stderr] = stdio;
        let (stdin, stdout, stderr) = (stdin?, stdout?, stderr?);
        let stdio = [
            child_stdio(stdin, 0, &mut keep)?,
            child_stdio(stdout, 1, &mut keep)?,
            child_stdio(stderr, 2, &mut keep)?,
        ];
        let own_network = spec.isolation == Isolation::Full && spec.network == Network::None;
        let mounts = mounts
            .into_iter()
            .map(|m| child_mount(m, own_network))
            .collect::<Result<_>>()?;
        let (root_remount, dev_remount) = match spec.isolation {
            // Unused: `Direct` mounts nothing.
            Isolation::Direct => (MountFlags::BIND, [MountFlags::BIND; DEV_NODES.len()]),
            Isolation::Full => (root_remount(&spec.root_dir)?, dev_remount()),
        };
        // Every mount under the root's bind is `nodev`: recursively where
        // the kernel can, else no mount may sit below the root.
        let root_nodev_rec = match spec.isolation {
            Isolation::Direct => false,
            Isolation::Full if mount_setattr_supported() => true,
            Isolation::Full => {
                refuse_submounts(&spec.root_dir)?;
                false
            }
        };
        let envp = env
            .iter()
            .map(|(k, v)| {
                let mut kv = k.as_bytes().to_vec();
                kv.push(b'=');
                kv.extend_from_slice(v.as_bytes());
                // The name only: the value may be a secret.
                CString::new(kv).map_err(|_| {
                    Error::InvalidSpec(format!("environment variable {k:?} contains a NUL byte"))
                })
            })
            .collect::<Result<_>>()?;
        let secrets = match (scratch, spec.secrets.is_empty()) {
            (_, true) => None,
            (None, false) => {
                return Err(Error::InvalidSpec(
                    "secret files need the full sandbox".into(),
                ));
            }
            (Some(dir), false) => Some(child_secrets(dir, &spec.secrets)?),
        };
        let mut flags = clone_flags(spec);
        let mut loopback = false;
        let mut hold_network = false;
        if spec.isolation == Isolation::Full && spec.network == Network::None {
            match *self
                .net_mode
                .get_or_init(|| probe_network(self.userns_network))
            {
                NetMode::Clone => loopback = true,
                NetMode::UserNs => {
                    flags &= !libc::CLONE_NEWNET;
                    hold_network = true;
                }
                NetMode::NotAllowed { direct } => {
                    return Err(Error::NoLoopback {
                        source: io::Error::from_raw_os_error(direct),
                    });
                }
                NetMode::Unavailable { direct, userns } => {
                    return Err(Error::NoLoopbackUserns {
                        direct: io::Error::from_raw_os_error(direct),
                        userns: io::Error::from_raw_os_error(userns),
                    });
                }
            }
        }
        let profile = match spec.isolation {
            Isolation::Full => self.seccomp.as_ref(),
            Isolation::Direct => None,
        };
        // A `RUN` in a user namespace's network namespace never runs
        // without its filter, installed before the profile so that the
        // profile cannot fake its install.
        let network_filter = if hold_network {
            let insns = seccomp::userns_network().ok_or_else(|| Error::NoLoopbackUserns {
                direct: io::Error::from_raw_os_error(libc::EPERM),
                userns: io::Error::from_raw_os_error(libc::ENOSYS),
            })?;
            Some(Program {
                insns: insns.into(),
                flags: 0,
            })
        } else {
            None
        };
        // A refused join fails a full-sandbox `RUN` (not the probe's) while
        // ranikust's session keyring is found and the `RUN`'s profile lets
        // `keyctl`, `add_key` or `request_key` run (ADR-0109).
        let keyring = match spec.isolation {
            Isolation::Full
                if self.probe || profile.is_some_and(SeccompProfile::refuses_keyrings) =>
            {
                KeyringJoin::Optional
            }
            Isolation::Full => KeyringJoin::Required,
            Isolation::Direct => KeyringJoin::Direct,
        };
        let mount_api =
            spec.isolation == Isolation::Full && self.mount_api.unwrap_or_else(mount_api_supported);
        let args = ChildArgs {
            root_dir: cstring(&spec.root_dir)?,
            root_remount,
            root_nodev_rec,
            mount_api,
            made: if mount_api {
                MadeMounts::default()
            } else {
                // Every bind `bind_remount` makes: the `RUN --mount`s, each
                // secret's targets, the read-only and masked kernel paths.
                MadeMounts::with_room(
                    spec.mounts.len()
                        + spec.secrets.len()
                        + plan::READONLY_PATHS.len()
                        + plan::MASKED_PATHS.len(),
                )
            },
            mounts,
            secrets,
            dev_remount,
            dev_pipes: plan::dev_pipes(spec),
            devpts: if self.own_ns.tty_gid {
                plan::DEVPTS
            } else {
                plan::DEVPTS_NO_TTY_GID
            },
            root_entry,
            hostname: hostname(spec).as_bytes().to_vec(),
            loopback,
            netns: None,
            // Only with a `NetNs`; `probe_network` checked it exists.
            network_filter,
            profile: profile.map(|p| p.program.clone()),
            keyring,
            drops: self.drops,
            keep: caps::docker_default_mask(),
            uid: Uid::from_raw(user.uid),
            gid: Gid::from_raw(user.gid),
            groups: user
                .supplementary_gids
                .iter()
                .copied()
                .map(Gid::from_raw)
                .collect(),
            set_groups: self.own_ns.setgroups,
            workdir: cstring(&spec.workdir)?,
            chroot,
            // Checked after the bind of the full sandbox's root.
            root_check: match spec.isolation {
                Isolation::Full => spec.root_id.map(|(maj, min, ino, _)| (maj, min, ino)),
                Isolation::Direct => None,
            },
            direct_drop: false,
            nofile: self.nofile.map(|l| rustix::process::Rlimit {
                current: l.soft,
                maximum: l.hard,
            }),
            core: crate::spec::original_core_limit(),
            stdio,
            argv: spec.argv.iter().map(cstring).collect::<Result<_>>()?,
            envp,
            candidates: candidates.iter().map(cstring).collect::<Result<_>>()?,
            script: None,
            // Set by `run` once the `RUN`'s cgroups are made.
            cgroup_init: None,
            cgroup_command: None,
        };
        Ok(Job {
            args,
            flags: libc::c_long::from(flags),
            isolation: spec.isolation,
            hold_network,
            fds: keep,
            user_ns: self.own_ns.user_ns,
        })
    }

    /// A journal id unique across processes sharing the state directory.
    fn next_id(&self) -> String {
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        format!(
            "{}-{:x}-{n}",
            rustix::process::getpid().as_raw_nonzero(),
            now.as_nanos()
        )
    }
}

/// Cleans up after a `RUN` that failed before its clone: the stubs, and
/// its cgroups, which no process joined.
fn abandon(journal: Option<Journal>, cgroup: Option<RunCgroup<'_>>) {
    if let Some(c) = cgroup
        && let Err(e) = c.remove()
    {
        tracing::warn!("rnk-sandbox: {e}");
    }
    if let Some(j) = journal
        && let Err(c) = j.finish()
    {
        tracing::warn!("rnk-sandbox: stub clean-up failed: {c}");
    }
}

/// `clone` flags of a `RUN`: the namespaces of `design/sandbox.md`, and
/// `SIGCHLD` as the exit signal.
fn clone_flags(spec: &RunSpec) -> libc::c_int {
    match spec.isolation {
        Isolation::Direct => libc::SIGCHLD,
        Isolation::Full => {
            let mut flags = libc::CLONE_NEWNS
                | libc::CLONE_NEWPID
                | libc::CLONE_NEWUTS
                | libc::CLONE_NEWIPC
                | libc::SIGCHLD;
            if spec.network == Network::None {
                flags |= libc::CLONE_NEWNET;
            }
            flags
        }
    }
}

/// Refuses a full-sandbox root whose `/proc`, `/sys` or `/dev` is a
/// symlink, before any stub is placed: `proc`, `sysfs` and the `/dev`
/// tmpfs are mounted only on a directory of the root itself, as runc
/// requires for `proc` and `sysfs`. Through a symlink, `proc` could land
/// below a directory that a later mount hides, out of reach of the masks.
/// The child checks again, without following any symlink.
fn refuse_symlinked_kernel_dirs(root: &RootPath, root_dir: &Path) -> Result<()> {
    for dir in [plan::PROC, plan::SYS, plan::DEV] {
        match root.statx(dir) {
            Ok(st) if FileType::from_raw_mode(st.mode) == FileType::Symlink => {
                return Err(Error::InvalidSpec(format!(
                    "{dir} in the root {} is a symbolic link; proc, sysfs and the /dev \
                     tmpfs are mounted only on a real directory",
                    root_dir.display()
                )));
            }
            Ok(_) => {}
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Places the stubs and writes the `/etc` copies.
fn place(journal: &mut Journal, spec: &RunSpec) -> Result<EtcFiles> {
    for (target, kind) in plan::mount_targets(spec) {
        journal.ensure(&target, kind)?;
    }
    let files = EtcFiles::in_dir(journal.scratch());
    let name = hostname(spec);
    // ranikust's own files, not the image's: what a `RUN` under Docker or
    // kaniko sees.
    let resolv_conf = read_host_file(Path::new("/etc/resolv.conf"))?;
    let hosts = read_host_file(Path::new("/etc/hosts"))?;
    write_scratch(
        &files.resolv_conf,
        resolv_conf.as_deref().unwrap_or_default(),
    )?;
    write_scratch(
        &files.hosts,
        &plan::hosts_file(spec.network, name, hosts.as_deref()),
    )?;
    write_scratch(&files.hostname, &plan::hostname_file(name))?;
    journal.commit()?;
    Ok(files)
}

/// Puts ranikust's `resolv.conf`, `hosts` and `hostname` and the `/dev`
/// nodes into an [`Isolation::Direct`] root other than `/`, journaled: what
/// a `RUN` under kaniko sees on `/` through the container's own mounts.
/// With no UTS namespace the host name is the real one. A missing `/etc` or
/// `/dev` is a stub; a node the kernel refuses (`EPERM`) is left out.
fn place_direct(journal: &mut Journal) -> Result<()> {
    for dir in ["/etc", "/dev"] {
        journal.ensure(Path::new(dir), MountKind::Directory)?;
    }
    let uname = rustix::system::uname();
    let name = uname.nodename().to_string_lossy();
    let resolv_conf = read_host_file(Path::new("/etc/resolv.conf"))?;
    let hosts = read_host_file(Path::new("/etc/hosts"))?;
    let hosts = plan::hosts_file(Network::Host, &name, hosts.as_deref());
    let hostname = plan::hostname_file(&name);
    let files: [(&str, &[u8]); 3] = [
        (
            "/etc/resolv.conf",
            resolv_conf.as_deref().unwrap_or_default(),
        ),
        ("/etc/hosts", &hosts),
        ("/etc/hostname", &hostname),
    ];
    for (path, data) in files {
        journal.swap_in(Path::new(path), Content::File(data))?;
    }
    for (path, major, minor) in plan::DIRECT_DEV_NODES {
        if !journal.swap_in(Path::new(path), Content::CharDev { major, minor })? {
            tracing::debug!("rnk-sandbox: {path} left out of the RUN: mknod refused");
        }
    }
    journal.commit()
}

/// The host's `/dev/null`, read-write, close-on-exec: reached without any
/// symlink or magic link and checked to be the null device (1:3). On `/` a
/// `Direct` `RUN` can replace it, say with a link to `/proc/self/fd/1`, which
/// would hand the next `RUN` ranikust's stdout.
fn host_null() -> Result<OwnedFd> {
    let p = Path::new("/dev/null");
    let fd = rustix::fs::openat2(
        rustix::fs::CWD,
        p,
        OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
        rustix::fs::ResolveFlags::NO_SYMLINKS | rustix::fs::ResolveFlags::NO_MAGICLINKS,
    )
    .map_err(|e| io_err("open", p, e.into()))?;
    let st = rustix::fs::fstat(&fd).map_err(|e| io_err("stat", p, e.into()))?;
    let rdev = (rustix::fs::major(st.st_rdev), rustix::fs::minor(st.st_rdev));
    if FileType::from_raw_mode(st.st_mode) != FileType::CharacterDevice || rdev != (1, 3) {
        return Err(Error::InvalidSpec(
            "/dev/null is not the null device (character device 1:3)".into(),
        ));
    }
    Ok(fd)
}

/// The remount of the root's bind: `nodev`, the root mount's flags kept.
fn root_remount(root_dir: &Path) -> Result<MountFlags> {
    let st = rustix::fs::statvfs(root_dir).map_err(|e| io_err("statvfs", root_dir, e.into()))?;
    Ok(plan::remount_flags(st.f_flag, MountFlags::NODEV))
}

/// Whether the kernel has `mount_setattr` (5.12) and lets this process call
/// it: a call with no valid argument fails with `EBADF`, `EINVAL` or
/// `EFAULT` where it exists, `ENOSYS` (or `EPERM` under a seccomp filter
/// that does not know it) where it does not. Probed once.
fn mount_setattr_supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        // SAFETY: null pointers with size 0; the kernel validates them and
        // fails before touching memory.
        let r = unsafe {
            libc::syscall(
                libc::SYS_mount_setattr,
                -1,
                std::ptr::null::<libc::c_char>(),
                0,
                std::ptr::null::<u8>(),
                0usize,
            )
        };
        let errno = io::Error::last_os_error().raw_os_error();
        r != 0 && !matches!(errno, Some(libc::ENOSYS | libc::EPERM))
    })
}

/// Whether the kernel has `open_tree` and `move_mount` (5.2) and lets this
/// process call them, probed once as [`mount_setattr_supported`]: a call
/// with no valid argument fails with `EBADF`, `EINVAL`, `EFAULT` or
/// `ENOENT` where it is allowed, `ENOSYS` or `EPERM` (a seccomp profile, or
/// no `CAP_SYS_ADMIN` over the mount namespace) where it is not.
fn mount_api_supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        let allowed = |r: libc::c_long| {
            let errno = io::Error::last_os_error().raw_os_error();
            r != 0 && !matches!(errno, Some(libc::ENOSYS | libc::EPERM))
        };
        // `OPEN_TREE_CLONE` of `<linux/mount.h>`: also checks the
        // privilege a clone needs, before the arguments.
        const OPEN_TREE_CLONE: libc::c_uint = 1;
        // SAFETY: an invalid descriptor and null paths; the kernel fails
        // before touching memory.
        let open_tree = unsafe {
            libc::syscall(
                libc::SYS_open_tree,
                -1,
                std::ptr::null::<libc::c_char>(),
                OPEN_TREE_CLONE,
            )
        };
        if !allowed(open_tree) {
            return false;
        }
        // SAFETY: as above.
        let move_mount = unsafe {
            libc::syscall(
                libc::SYS_move_mount,
                -1,
                std::ptr::null::<libc::c_char>(),
                -1,
                std::ptr::null::<libc::c_char>(),
                0u32,
            )
        };
        allowed(move_mount)
    })
}

/// Refuses `root_dir` when a mount sits below it (`/proc/self/mountinfo`):
/// without `mount_setattr` only the top mount of the root's bind becomes
/// `nodev`, so a device node on a submount would open.
fn refuse_submounts(root_dir: &Path) -> Result<()> {
    // This thread's namespace: the one the child's is copied from.
    let p = Path::new("/proc/thread-self/mountinfo");
    let text = fs::read(p).map_err(|e| io_err("read", p, e))?;
    match submount_below(&text, root_dir.as_os_str().as_bytes()) {
        Some(m) => Err(Error::InvalidSpec(format!(
            "the root {} has a mount below it ({}); without mount_setattr (Linux 5.12) it \
             cannot be made nodev",
            root_dir.display(),
            String::from_utf8_lossy(&m)
        ))),
        None => Ok(()),
    }
}

/// The first mount point of `mountinfo` strictly below `root` (absolute,
/// clean), unescaped.
fn submount_below(mountinfo: &[u8], root: &[u8]) -> Option<Vec<u8>> {
    let prefix: Vec<u8> = if root == b"/" {
        b"/".to_vec()
    } else {
        [root, b"/"].concat()
    };
    mountinfo
        .split(|&b| b == b'\n')
        .filter_map(|line| line.split(|&b| b == b' ').nth(4))
        .map(unescape_octal)
        .find(|m| m.len() > prefix.len() && m.starts_with(&prefix))
}

/// Undoes mountinfo's `\ooo` escapes (space, tab, newline, backslash).
fn unescape_octal(field: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(field.len());
    let mut i = 0;
    while i < field.len() {
        let oct = field
            .get(i + 1..i + 4)
            .filter(|d| field[i] == b'\\' && d.iter().all(|c| (b'0'..=b'7').contains(c)));
        match oct {
            Some(d) => {
                out.push(
                    d.iter()
                        .fold(0u8, |a, c| a.wrapping_mul(8).wrapping_add(c - b'0')),
                );
                i += 4;
            }
            None => {
                out.push(field[i]);
                i += 1;
            }
        }
    }
    out
}

/// The read-only remount of each `/dev` node bind, the outer mount's flags
/// kept (never `nodev`: the nodes must open).
fn dev_remount() -> [MountFlags; DEV_NODES.len()] {
    let add = MountFlags::RDONLY | MountFlags::NOSUID | MountFlags::NOEXEC;
    DEV_NODES.map(|(_, source)| match rustix::fs::statvfs(source) {
        Ok(st) => plan::remount_flags(st.f_flag, add),
        // Missing: the child leaves the node out.
        Err(_) => MountFlags::BIND | add,
    })
}

fn hostname(spec: &RunSpec) -> &str {
    if spec.hostname.is_empty() {
        DEFAULT_HOSTNAME
    } else {
        &spec.hostname
    }
}

/// Whether the child binds the kernel filesystem instead of mounting one;
/// for `proc`, also whether a `RUN` runs alone and `Direct` is refused
/// (ADR-0127).
fn is_rbind(how: KernelFs) -> bool {
    match how {
        KernelFs::Fresh => false,
        KernelFs::Rbind => true,
    }
}

/// Why `Direct` is refused where `RUN`s get an rbind of `/proc`.
const DIRECT_UNDER_RBIND: &str =
    "Isolation::Direct where RUNs get an rbind of /proc: use Isolation::Full";

/// `op` as the child performs it; `own_network` says the `RUN` has a
/// network namespace of its own, for which an rbind of `/sys` tries a new
/// `sysfs` first.
fn child_mount(op: MountOp, own_network: bool) -> Result<ChildMount> {
    Ok(match op {
        MountOp::Proc { target, how } => ChildMount::Proc {
            target: cstring(&target)?,
            rbind: is_rbind(how),
        },
        MountOp::Sysfs { target, how } => ChildMount::Sysfs {
            target: cstring(&target)?,
            rbind: is_rbind(how),
            fresh_first: is_rbind(how) && own_network,
        },
        MountOp::Dev { target } => ChildMount::Dev {
            target: cstring(&target)?,
        },
        MountOp::File { source, target } => {
            check_source(&source, MountKind::File)?;
            ChildMount::File {
                source: cstring(&source)?,
                target: cstring(&target)?,
            }
        }
        MountOp::Bind {
            source,
            target,
            kind,
            read_only,
        } => {
            let st = check_source(&source, kind)?;
            let mut add = MountFlags::NODEV;
            if read_only {
                add |= MountFlags::RDONLY;
            }
            ChildMount::Bind {
                source: cstring(&source)?,
                dir: kind == MountKind::Directory,
                target: cstring(&target)?,
                remount: plan::remount_flags(st.f_flag, add),
            }
        }
    })
}

/// Checks a mount source as the child will (no symlink as the last
/// component, the type of `kind`), for an error that names it, and returns
/// its mount's flags. The child opens it again in its own mount namespace.
fn check_source(p: &Path, kind: MountKind) -> Result<rustix::fs::StatVfs> {
    const OP: &str = "open a mount source";
    let fd = rustix::fs::open(
        p,
        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| io_err(OP, p, e.into()))?;
    let st = rustix::fs::fstat(&fd).map_err(|e| io_err(OP, p, e.into()))?;
    let want = match kind {
        MountKind::Directory => FileType::Directory,
        MountKind::File => FileType::RegularFile,
    };
    let found = FileType::from_raw_mode(st.st_mode);
    if found != want {
        return Err(Error::InvalidSpec(format!(
            "mount source {p:?} is a {found:?}, not a {want:?}"
        )));
    }
    rustix::fs::fstatvfs(&fd).map_err(|e| io_err("statvfs", p, e.into()))
}

/// A caller's stream `target` (0, 1 or 2) for the child: a descriptor is
/// duplicated to 3 or higher, close-on-exec, and kept open in `keep` until
/// the clone. A descriptor, or ranikust's own stream for `Inherit`, must
/// not be a directory or an `O_PATH` descriptor ([`check_stream`]).
fn child_stdio(s: Stdio, target: RawFd, keep: &mut Vec<OwnedFd>) -> Result<ChildStdio> {
    Ok(match s {
        Stdio::Inherit => {
            // SAFETY: `F_GETFL` on a descriptor number; a closed one fails
            // with EBADF.
            if unsafe { libc::fcntl(target, libc::F_GETFL) } >= 0 {
                // SAFETY: open, as just checked, and never closed by this
                // process: it is one of its standard streams.
                let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(target) };
                check_stream(fd, target)?;
            }
            ChildStdio::Inherit
        }
        Stdio::Null => ChildStdio::Null,
        Stdio::Fd(fd) => {
            check_stream(std::os::fd::AsFd::as_fd(&fd), target)?;
            let dup = rustix::io::fcntl_dupfd_cloexec(&fd, 3)
                .map_err(|e| sys_err("duplicate a stdio descriptor", e))?;
            let raw: RawFd = dup.as_raw_fd();
            keep.push(dup);
            ChildStdio::Fd(raw)
        }
    })
}

/// Refuses a `RUN` stream that is a directory or an `O_PATH` descriptor: it
/// crosses `execve`, and `/proc/self/fd/<n>` or `openat` from it would let
/// the `RUN` look up paths outside its root.
fn check_stream(fd: std::os::fd::BorrowedFd<'_>, target: RawFd) -> Result<()> {
    let name = ["stdin", "stdout", "stderr"]
        .get(usize::try_from(target).unwrap_or(usize::MAX))
        .copied()
        .unwrap_or("stream");
    let flags = rustix::fs::fcntl_getfl(fd).map_err(|e| sys_err("fcntl(F_GETFL)", e))?;
    if flags.contains(OFlags::PATH) {
        return Err(Error::InvalidSpec(format!(
            "the RUN's {name} is an O_PATH descriptor"
        )));
    }
    let st = rustix::fs::fstat(fd).map_err(|e| sys_err("fstat a stdio descriptor", e))?;
    if FileType::from_raw_mode(st.st_mode) == FileType::Directory {
        return Err(Error::InvalidSpec(format!(
            "the RUN's {name} is a directory"
        )));
    }
    Ok(())
}

fn read_host_file(p: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(p) {
        Ok(data) => Ok(Some(data)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_err("read", p, e)),
    }
}

/// Writes a run's file, mode 0644 whatever the umask, so any `USER` can read
/// it through the bind.
fn write_scratch(p: &Path, data: &[u8]) -> Result<()> {
    let mut f = File::options()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(p)
        .map_err(|e| io_err("create", p, e))?;
    f.write_all(data).map_err(|e| io_err("write", p, e))?;
    f.set_permissions(Permissions::from_mode(0o644))
        .map_err(|e| io_err("chmod", p, e))
}

/// For arguments, paths and names, never an environment value.
fn cstring(s: impl AsRef<OsStr>) -> Result<CString> {
    let s = s.as_ref();
    CString::new(s.as_bytes())
        .map_err(|_| Error::InvalidSpec(format!("contains a NUL byte: {s:?}")))
}

/// The child's secrets: files `0`, `1`… on a tmpfs over `dir`, one per
/// secret allocation, owner and mode, each bound at all its targets; the
/// tmpfs is sized for them plus a page each, mode 0700.
fn child_secrets(dir: &Path, secrets: &[SecretFile]) -> Result<ChildSecrets> {
    let mut files: Vec<ChildSecret> = Vec::new();
    for s in secrets {
        let uid = chown_id(s.uid).map(Uid::from_raw);
        let gid = chown_id(s.gid).map(Gid::from_raw);
        let mode = Mode::from_raw_mode(s.mode & 0o777);
        let target = cstring(&s.target)?;
        match files.iter_mut().find(|f| {
            std::sync::Arc::ptr_eq(&f.data, &s.data) && (f.uid, f.gid, f.mode) == (uid, gid, mode)
        }) {
            Some(f) => f.targets.push(target),
            None => files.push(ChildSecret {
                name: cstring(files.len().to_string())?,
                data: std::sync::Arc::clone(&s.data),
                uid,
                gid,
                mode,
                targets: vec![target],
            }),
        }
    }
    let bytes: usize = files.iter().map(|f| f.data.len() + 4096).sum();
    let options = format!("size={},mode=0700", bytes.max(16 * 1024));
    Ok(ChildSecrets {
        dir: cstring(dir)?,
        options: cstring(options)?,
        files,
    })
}

/// The turns of the process's `RUN`s (ADR-0127). One that gets the outer
/// `/proc` runs alone: through it `RUN`s of one user namespace could reach
/// each other's processes, whichever [`Sandbox`] runs them. The others
/// share theirs. A spawner thread lost during a turn breaks them, as a
/// panic does; neither happens today (nothing after the clone panics,
/// and release builds abort).
static TURNS: Turns = Turns::new();

#[derive(Debug)]
struct Turns {
    lock: RwLock<()>,
    // Set by a panic during a turn (a shared turn's panic does not poison
    // `lock`) or by a spawner thread lost during one.
    broken: AtomicBool,
}

/// A `RUN`'s turn, until it drops.
struct Turn<'a> {
    _held: Held<'a>,
    broken: &'a AtomicBool,
}

enum Held<'a> {
    Alone { _guard: RwLockWriteGuard<'a, ()> },
    Shared { _guard: RwLockReadGuard<'a, ()> },
}

impl Turns {
    const fn new() -> Self {
        Self {
            lock: RwLock::new(()),
            broken: AtomicBool::new(false),
        }
    }

    /// Waits for the turn of a `RUN` run with `isolation` where `RUN`s get
    /// `/proc` as `proc`; a `Direct` one there is refused at once, as
    /// [`Runner::run`] refuses it too.
    fn take(&self, proc: KernelFs, isolation: Isolation) -> Result<Turn<'_>> {
        let held = match (isolation, is_rbind(proc)) {
            (Isolation::Direct, true) => {
                return Err(Error::InvalidSpec(DIRECT_UNDER_RBIND.into()));
            }
            (Isolation::Full, true) => Held::Alone {
                _guard: self.lock.write().map_err(|_| Error::TurnPoisoned)?,
            },
            (Isolation::Full | Isolation::Direct, false) => Held::Shared {
                _guard: self.lock.read().map_err(|_| Error::TurnPoisoned)?,
            },
        };
        // A turn must outlast its `RUN`'s processes; one a panic ended may
        // not have, so no turn follows it.
        if self.broken.load(Ordering::SeqCst) {
            return Err(Error::TurnPoisoned);
        }
        Ok(Turn {
            _held: held,
            broken: &self.broken,
        })
    }
}

impl Turn<'_> {
    /// Refuses every later turn.
    fn break_turns(&self) {
        self.broken.store(true, Ordering::SeqCst);
    }
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.break_turns();
        }
    }
}

/// The `RUN`s running now, for [`Sandbox::cancel`].
#[derive(Debug, Default)]
pub(crate) struct Running(Mutex<RunningState>);

#[derive(Debug, Default)]
struct RunningState {
    cancelled: bool,
    pids: Vec<(Pid, Isolation, Option<Killer>)>,
}

impl Running {
    fn state(&self) -> MutexGuard<'_, RunningState> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Kills every running `RUN`, and every later one as soon as it is
    /// cloned.
    fn cancel(&self) {
        let mut state = self.state();
        state.cancelled = true;
        for (pid, isolation, killer) in &state.pids {
            kill_run(*pid, *isolation, killer.as_ref());
        }
    }

    fn is_cancelled(&self) -> bool {
        self.state().cancelled
    }

    /// Records a cloned child and its cgroup's killer; kills it at once
    /// after a cancel.
    fn add(&self, pid: Pid, isolation: Isolation, killer: Option<Killer>) {
        let mut state = self.state();
        if state.cancelled {
            kill_run(pid, isolation, killer.as_ref());
        }
        state.pids.push((pid, isolation, killer));
    }

    /// Forgets a child that has exited and is not reaped yet, so that its pid
    /// cannot have been reused; returns whether the build was cancelled.
    fn remove(&self, pid: Pid) -> bool {
        let mut state = self.state();
        state.pids.retain(|&(p, _, _)| p != pid);
        state.cancelled
    }
}

/// `SIGKILL` to a `RUN`, as BuildKit kills the process on a cancel
/// (`executor/runcexecutor/executor.go`). A `Full` child is the `RUN`'s
/// init, PID 1 of its namespace, so the kernel kills the rest, the command
/// included (ADR-0080); with a cgroup, `cgroup.kill` first reaches the
/// command's processes at once, however little CPU the init gets
/// (ADR-0110). A `Direct` one leads its process group. Called only while the
/// child is not reaped, so the pid is its own; `ESRCH` (already gone) is no
/// error.
fn kill_run(pid: Pid, isolation: Isolation, killer: Option<&Killer>) {
    if let Some(k) = killer
        && let Err(e) = k.kill()
    {
        // The init's `SIGKILL` below still ends the `RUN`; its cgroup's
        // removal reports the failure again.
        tracing::debug!("rnk-sandbox: cgroup.kill of a cancelled RUN: {e}");
    }
    // Nothing to report: the RUN is gone either way, and the caller's reap
    // says how it ended.
    let _ = rustix::process::kill_process(pid, Signal::KILL);
    if isolation == Isolation::Direct {
        // No group yet when the child has not reached `setpgid`; the kill of
        // the pid above covers it.
        let _ = rustix::process::kill_process_group(pid, Signal::KILL);
    }
}

/// Clones the child, learns whether it reached `execve`, and reaps it. With
/// `running`, the child can be killed by [`Sandbox::cancel`] until it exits,
/// through `killer` too when it has a cgroup. With `sweep` (a
/// [`Sandbox::start_direct`] sandbox: a subreaper running one `RUN` at a
/// time), what the `RUN` left is killed and reaped too.
fn execute(
    job: Job,
    spawn: impl FnOnce(Job) -> Result<Spawned>,
    program: &str,
    running: Option<&Running>,
    sweep: bool,
    killer: Option<Killer>,
) -> Result<(ExitStatus, libc::rusage)> {
    let isolation = job.isolation;
    // ranikust's own filters answer with an errno: a `SIGSYS` comes from a
    // profile's kill or trap.
    let profile = job.args.profile.is_some();
    // What a refused switch to the user names ([`unmapped_user`]).
    let user = RunUser {
        uid: job.args.uid,
        gid: job.args.gid,
        groups: if job.args.set_groups {
            job.args.groups.clone()
        } else {
            Vec::new()
        },
        user_ns: job.user_ns,
    };
    // And what a refused owner of a secret file names ([`unmapped_secret`]).
    let secrets = secret_owners(job.args.secrets.as_ref());
    if running.is_some_and(Running::is_cancelled) {
        return Err(Error::Cancelled);
    }
    // ranikust's own children before the `RUN`: whatever else becomes its
    // child while the `RUN` runs is the `RUN`'s, reparented to the
    // subreaper (ADR-0051).
    let sweep = sweep && isolation == Isolation::Direct;
    let helpers = if sweep {
        sweep::children_of_self()?
    } else {
        Vec::new()
    };
    let Spawned {
        pid,
        failures,
        status: command_status,
        netns,
    } = spawn(job)?;
    if let Some(r) = running {
        r.add(pid, isolation, killer.clone());
    }
    // The child is reaped whatever the read, the wait or the kill gave.
    let failure = read_failure(&failures);
    drop(failures);
    // Waits for the exit without reaping: until the reap, the pid cannot be
    // reused, so neither a cancel nor the group kill can reach another
    // process.
    // A wait that fails for another reason than a signal must not leave a
    // child running after `run` returns (its writes would reach the layer):
    // kill it while it is still registered, and wait again. A second failure
    // fails the RUN, and so the build.
    let exited = wait_exited(pid).or_else(|e| match &e {
        // Not our child (any more): its pid may be another process's now,
        // so nothing is killed.
        Error::Sys { source, .. } if source.raw_os_error() == Some(libc::ECHILD) => Err(e),
        _ => {
            kill_run(pid, isolation, killer.as_ref());
            wait_exited(pid).map_err(|_| e)
        }
    });
    let cancelled = running.is_some_and(|r| r.remove(pid));
    let killed = match (exited, isolation) {
        (Ok(()), Isolation::Direct) if sweep => {
            kill_leftovers(pid).and_then(|()| sweep::kill_run_tree(pid, &helpers))
        }
        (Ok(()), Isolation::Direct) => kill_leftovers(pid),
        // A `Direct` child whose wait failed: `kill_run` already killed its
        // group, unless the pid is no longer ours (`ECHILD`); the RUN fails,
        // and so the build. Descendants that left the group (`setsid`) are
        // not swept on this path.
        (r @ Err(_), Isolation::Direct) => r,
        // The child is the init, PID 1 of its namespace: when it exits, once
        // the command has, the kernel kills the rest.
        (r, Isolation::Full) => r,
    };
    let reaped = reap(pid);
    // The child is reaped: its namespace can end with this descriptor.
    drop(netns);
    let (status, usage) = reaped?;
    killed?;
    // A full-sandbox child is the `RUN`'s init: the command's own status is
    // what it wrote, if it lived to write it; without it, `status` is the
    // init's own, which `ending` never returns as the command's.
    let mut unreported = false;
    let status = match command_status.map(|fd| read_status(&fd)) {
        Some(Ok(Some(raw))) => ExitStatus::from_raw(raw),
        Some(Ok(None)) => {
            unreported = true;
            status
        }
        // The read's error fails the `RUN`; the rest of its end is logged, not dropped.
        Some(Err(e)) => {
            tracing::debug!(
                record = ?failure,
                init = %status,
                cancelled,
                "rnk-sandbox: a RUN's exit status could not be read"
            );
            return Err(e);
        }
        None => status,
    };
    if cancelled {
        return Err(Error::Cancelled);
    }
    // `Setup` and `Exec` come from the child before `execve` and are
    // returned only here, after the tree is reaped and swept (`killed?`):
    // `Error::ran_no_command` relies on it (ADR-0056). Keep it so.
    let failure = failure.inspect_err(|_| {
        if unreported {
            tracing::debug!(
                %status,
                "rnk-sandbox: a RUN's init ended without the command's status; its set-up record could not be read"
            );
        }
    })?;
    let item = failure.map_or(0, |(_, item, _)| item);
    let failure = failure.map(|(step, _, errno)| (step, errno));
    ending(status, unreported, failure, profile, program)
        .map_err(|e| unmapped_user(e, &user))
        .map_err(|e| unmapped_secret(e, item, &secrets, user.user_ns))
        .map(|s| (s, usage))
}

/// A secret file of a `RUN`, as a refused `chown` of it names it: the
/// first target it is bound at, and its owner.
#[derive(Debug)]
struct SecretOwner {
    target: PathBuf,
    uid: Option<u32>,
    gid: Option<u32>,
}

/// The owners of `secrets`' files, in the child's order: the item of a
/// record of [`Step::SecretOwner`] is an index here.
fn secret_owners(secrets: Option<&ChildSecrets>) -> Vec<SecretOwner> {
    let files = secrets.map_or(&[][..], |s| &s.files);
    files
        .iter()
        .map(|f| SecretOwner {
            target: f
                .targets
                .first()
                .map(|t| PathBuf::from(OsStr::from_bytes(t.as_bytes())))
                .unwrap_or_default(),
            uid: f.uid.map(Uid::as_raw),
            gid: f.gid.map(Gid::as_raw),
        })
        .collect()
}

/// `e`, or, for the `chown` of the secret file `item` of `secrets` refused
/// with `EINVAL` in a user namespace other than the initial one
/// (`user_ns`), [`Error::Unmapped`] naming the file and its owner
/// (ADR-0098): `fchown` returns it for an id the namespace does not map
/// alone, and every id but `-1` maps in the initial one. An item past the
/// files, or the child's `u16::MAX` for one it could not number, names
/// nothing.
fn unmapped_secret(e: Error, item: u16, secrets: &[SecretOwner], user_ns: bool) -> Error {
    if let Error::Setup {
        step: Step::SecretOwner,
        source,
    } = &e
        && source.raw_os_error() == Some(libc::EINVAL)
        && user_ns
        && item != u16::MAX
        && let Some(s) = secrets.get(usize::from(item))
    {
        return Error::Unmapped(rnk_rootfs::Unmapped::chown(&s.target, s.uid, s.gid));
    }
    e
}

/// The ids a `RUN` switches to before `execve`.
struct RunUser {
    uid: Uid,
    gid: Gid,
    /// The supplementary groups, when they are set.
    groups: Vec<Gid>,
    /// The switch is made in a user namespace other than the initial one.
    user_ns: bool,
}

/// The kernel's `NGROUPS_MAX` (`include/uapi/linux/limits.h`, 65536 since
/// Linux 2.6.4): `setgroups` refuses a longer list with `EINVAL` too.
const NGROUPS_MAX: usize = 65536;

/// `e`, or, for a switch to `user` refused with `EINVAL`, which
/// `setgroups`, `setresgid` and `setresuid` return for an id the user
/// namespace does not map, [`Error::Unmapped`] naming the ids (ADR-0098).
/// Only where no other cause is left: in the initial user namespace every
/// id maps, and `setgroups` refuses more than [`NGROUPS_MAX`] groups with
/// the same errno.
fn unmapped_user(e: Error, user: &RunUser) -> Error {
    if let Error::Setup {
        step: step @ (Step::Groups | Step::Gid | Step::Uid),
        source,
    } = &e
        && source.raw_os_error() == Some(libc::EINVAL)
        && user.user_ns
        && user.groups.len() <= NGROUPS_MAX
    {
        let groups: Vec<u32> = user.groups.iter().map(|g| g.as_raw()).collect();
        return Error::Unmapped(rnk_rootfs::Unmapped::user(
            *step,
            user.uid.as_raw(),
            user.gid.as_raw(),
            &groups,
        ));
    }
    e
}

/// How a reaped `RUN` that was not cancelled ended: its status, the
/// failure the child recorded before `execve`, or, where the full sandbox's
/// init ended without the command's status (`unreported`: `status` is then
/// the init's own), [`Error::SetupKilled`] for a signal, which names a
/// profile behind a `SIGSYS`, or [`Error::InitExited`] for a code other
/// than 0. Code 0, which the init exits with only after its status write,
/// means that write was lost: a status that could not be read
/// ([`STATUS_OP`]). A record wins over the init's code, 0 included, which
/// is logged; between a record and the init's signal, the more precise one
/// is returned and the other logged.
fn ending(
    status: ExitStatus,
    unreported: bool,
    failure: Option<(Step, i32)>,
    profile: bool,
    program: &str,
) -> Result<ExitStatus> {
    let killed = |signal| Error::SetupKilled {
        signal,
        seccomp: profile && signal == libc::SIGSYS,
    };
    let init_killed_by = status.signal().filter(|_| unreported);
    match (failure, init_killed_by) {
        // Not killed, so exited: `reap` waits for nothing else.
        (None, None) if unreported => match libc::WEXITSTATUS(status.into_raw()) {
            0 => Err(sys_err(
                STATUS_OP,
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "the RUN's init exited without the command's status",
                ),
            )),
            code => Err(Error::InitExited { code }),
        },
        (None, None) => Ok(status),
        (None, Some(signal)) => Err(killed(signal)),
        // The init's end ends the go pipe, which a command still waiting
        // reports (`EPIPE`): a record of `Step::Init` may be its consequence.
        (Some((Step::Init, errno)), Some(signal)) => {
            tracing::debug!(
                errno,
                signal,
                "rnk-sandbox: a RUN's init was killed by a signal; its failure record of the init step is not returned"
            );
            Err(killed(signal))
        }
        (Some((step, errno)), signal) => {
            if let Some(signal) = signal {
                tracing::debug!(
                    %step,
                    signal,
                    "rnk-sandbox: a RUN's init was also killed by a signal"
                );
            } else if unreported {
                tracing::debug!(
                    %step,
                    %status,
                    "rnk-sandbox: a RUN's init also ended without the command's status"
                );
            }
            let source = io::Error::from_raw_os_error(errno);
            Err(match step {
                Step::Exec => Error::Exec {
                    program: program.to_owned(),
                    source,
                },
                step => Error::Setup { step, source },
            })
        }
    }
}

/// The child's failure record (step, item, errno), or `None` at end-of-file
/// (it executed the command, which closed the pipe).
fn read_failure(fd: &OwnedFd) -> Result<Option<(Step, u16, i32)>> {
    const OP: &str = "read the RUN set-up result";
    let mut record = [0u8; 8];
    let mut len = 0;
    while let Some(rest) = record.get_mut(len..).filter(|r| !r.is_empty()) {
        match rustix::io::read(fd, rest) {
            Ok(0) => break,
            Ok(n) => len += n,
            Err(Errno::INTR) => {}
            Err(e) => return Err(sys_err(OP, e)),
        }
    }
    match len {
        0 => Ok(None),
        8 => child::decode_failure(record)
            .map(Some)
            .ok_or_else(|| sys_err(OP, io::Error::from(io::ErrorKind::InvalidData))),
        _ => Err(sys_err(OP, io::Error::from(io::ErrorKind::UnexpectedEof))),
    }
}

/// The operation of the errors for a full-sandbox `RUN`'s status that could
/// not be read: a status record that failed to read, was cut short or holds
/// no end, or none from an init that exited 0.
const STATUS_OP: &str = "read the RUN's exit status";

/// The command's wait status the `RUN`'s init wrote, or `None` when it
/// wrote nothing (end-of-file).
fn read_status(fd: &OwnedFd) -> Result<Option<i32>> {
    let mut record = [0u8; child::STATUS_LEN];
    let mut len = 0;
    while let Some(rest) = record.get_mut(len..).filter(|r| !r.is_empty()) {
        match rustix::io::read(fd, rest) {
            Ok(0) => break,
            Ok(n) => len += n,
            Err(Errno::INTR) => {}
            Err(e) => return Err(sys_err(STATUS_OP, e)),
        }
    }
    match len {
        0 => Ok(None),
        child::STATUS_LEN => {
            let raw = i32::from_le_bytes(record);
            // Only an end: the init never reports a stop.
            if libc::WIFEXITED(raw) || libc::WIFSIGNALED(raw) {
                Ok(Some(raw))
            } else {
                Err(sys_err(
                    STATUS_OP,
                    io::Error::from(io::ErrorKind::InvalidData),
                ))
            }
        }
        _ => Err(sys_err(
            STATUS_OP,
            io::Error::from(io::ErrorKind::UnexpectedEof),
        )),
    }
}

/// Waits until `pid` has exited, without reaping it: while it is a zombie,
/// its pid cannot be reused.
fn wait_exited(pid: Pid) -> Result<()> {
    loop {
        match rustix::process::waitid(
            WaitId::Pid(pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOWAIT,
        ) {
            Ok(_) => return Ok(()),
            Err(Errno::INTR) => {}
            Err(e) => return Err(sys_err("waitid", e)),
        }
    }
}

/// Whether a `Direct` child on `/` drops capabilities from its bounding set
/// (ADR-0051): those outside Docker's defaults, where ranikust holds
/// `CAP_SETPCAP`. Without it, any capability outside the defaults left in
/// the bounding set stops the `RUN`. The child trims its ambient,
/// inheritable, permitted and effective sets to the defaults in every case,
/// which needs no privilege.
fn direct_slash_drop() -> Result<bool> {
    // Only a root parent gives its `RUN` the bounding set as permitted
    // capabilities; kaniko mode always runs as root. A non-root process
    // (a test) keeps a full bounding set it never passes on.
    if !rustix::process::geteuid().is_root() {
        return Ok(false);
    }
    let bounding = caps::bounding_set(caps::last_cap());
    let setpcap = rustix::thread::capabilities(None)
        .map_err(|e| sys_err("read the capability sets", e))?
        .effective
        .contains(rustix::thread::CapabilitySet::SETPCAP);
    match caps::direct_caps(bounding, setpcap) {
        caps::DirectCaps::Keep => Ok(false),
        caps::DirectCaps::Drop => Ok(true),
        caps::DirectCaps::Refuse(names) => Err(Error::RunReachesState {
            caps: names.join(", "),
        }),
    }
}

/// A `#!` heredoc of a `Direct` `RUN` on `/` in a memfd (ADR-0057): sealed
/// against any change, close-on-exec until the child clears it, numbered 3
/// or higher. Executable memfds need `MFD_EXEC` where the kernel knows it
/// (6.3+, else `EINVAL` and the default, executable, is asked for); where
/// `vm.memfd_noexec` forbids them, the `RUN` is refused rather than given a
/// named file in the root. The interpreter reads it as `/dev/fd/<n>`: the
/// container's `/dev/fd` must lead to `/proc/self/fd`.
fn script_memfd(body: &[u8]) -> Result<OwnedFd> {
    use rustix::fs::{MemfdFlags, SealFlags};
    let dev_fd = rustix::fs::statx(
        rustix::fs::CWD,
        "/dev/fd",
        rustix::fs::AtFlags::empty(),
        rustix::fs::StatxFlags::TYPE,
    );
    if !matches!(&dev_fd, Ok(st) if rustix::fs::FileType::from_raw_mode(st.stx_mode.into()) == rustix::fs::FileType::Directory)
    {
        return Err(Error::Direct(
            "a #! heredoc runs from /dev/fd/<n>, and /dev/fd is not a directory here; the \
             container needs /dev/fd -> /proc/self/fd, as Docker and Kubernetes set"
                .into(),
        ));
    }
    let flags = MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING;
    let fd = match rustix::fs::memfd_create("ranikust-heredoc", flags | MemfdFlags::EXEC) {
        Err(Errno::INVAL) => rustix::fs::memfd_create("ranikust-heredoc", flags),
        r => r,
    }
    .map_err(|e| {
        Error::Direct(format!(
            "a #! heredoc runs from an executable memfd, which this kernel refuses ({e}; \
             vm.memfd_noexec=2?)"
        ))
    })?;
    let mut rest = body;
    while !rest.is_empty() {
        match rustix::io::write(&fd, rest) {
            Ok(n) => rest = &rest[n..],
            Err(Errno::INTR) => {}
            Err(e) => return Err(sys_err("write the heredoc script", e)),
        }
    }
    rustix::fs::fcntl_add_seals(
        &fd,
        SealFlags::SEAL | SealFlags::SHRINK | SealFlags::GROW | SealFlags::WRITE,
    )
    .map_err(|e| sys_err("seal the heredoc script", e))?;
    if fd.as_raw_fd() < 3 {
        return rustix::io::fcntl_dupfd_cloexec(&fd, 3)
            .map_err(|e| sys_err("duplicate the heredoc script", e));
    }
    Ok(fd)
}

/// Kills what a `Direct` command left in its process group, as kaniko does,
/// once the command itself has exited ([`wait_exited`]): the leader is a
/// zombie, so its pid, the group's id, is not reused.
fn kill_leftovers(pid: Pid) -> Result<()> {
    match rustix::process::kill_process_group(pid, Signal::KILL) {
        // No group: the child failed before `setpgid`.
        Ok(()) | Err(Errno::SRCH) => Ok(()),
        Err(e) => Err(sys_err("kill the RUN process group", e)),
    }
}

/// Reaps `pid` with its resource usage, which covers the descendants it
/// waited for.
fn reap(pid: Pid) -> Result<(ExitStatus, libc::rusage)> {
    let raw = pid.as_raw_nonzero().get();
    let mut status: libc::c_int = 0;
    // SAFETY: `rusage` is integers and `timeval`s; all-zero is valid.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: `wait4` writes the status and the usage, both live across
        // the call.
        let r = unsafe { libc::wait4(raw, &raw mut status, 0, &raw mut usage) };
        if r == raw {
            return Ok((ExitStatus::from_raw(status), usage));
        }
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EINTR) {
            return Err(sys_err("wait4", e));
        }
    }
}

fn record_usage(span: &Span, u: &libc::rusage) {
    span.record("user_ns", timeval_ns(u.ru_utime));
    span.record("sys_ns", timeval_ns(u.ru_stime));
    // Linux reports `ru_maxrss` in KiB.
    span.record("max_rss_kb", count(u.ru_maxrss));
    span.record("minflt", count(u.ru_minflt));
    span.record("majflt", count(u.ru_majflt));
    span.record("inblock", count(u.ru_inblock));
    span.record("oublock", count(u.ru_oublock));
    span.record("nvcsw", count(u.ru_nvcsw));
    span.record("nivcsw", count(u.ru_nivcsw));
}

fn count(v: impl Into<i64>) -> u64 {
    u64::try_from(v.into()).unwrap_or(0)
}

fn timeval_ns(t: libc::timeval) -> u64 {
    count(t.tv_sec)
        .saturating_mul(1_000_000_000)
        .saturating_add(count(t.tv_usec).saturating_mul(1_000))
}

#[cfg(test)]
mod cgroup_tests;
#[cfg(test)]
mod tests {
    use super::*;

    /// The inode of the network namespace `fd` refers to.
    fn ns_inode(fd: &OwnedFd) -> u64 {
        rustix::fs::fstat(fd).unwrap().st_ino
    }

    /// A namespace in a user namespace of its own: unprivileged where the
    /// host allows user namespaces, skipped where it does not. The helper
    /// is gone; the descriptor alone keeps the namespace.
    #[test]
    fn a_created_network_namespace_outlives_its_helper() {
        let ns = match NetNs::create(true) {
            Ok(ns) => ns,
            Err(e) => {
                eprintln!("skipped: no user namespace here ({e})");
                return;
            }
        };
        let mine = rustix::fs::open(
            "/proc/self/ns/net",
            OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap();
        assert_ne!(ns_inode(&ns.fd), ns_inode(&mine));
    }

    /// Joins `ns` in a forked child and reports whether `lo` is up there:
    /// its `127.0.0.1` local route exists only then.
    fn loopback_is_up_in(ns: &NetNs) -> bool {
        let fd = ns.fd.as_raw_fd();
        // SAFETY: the child only makes syscalls on stack buffers and exits
        // with `_exit`.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            // SAFETY: as above.
            unsafe {
                if libc::setns(fd, libc::CLONE_NEWNET) != 0 {
                    libc::_exit(2);
                }
                let f = libc::open(c"/proc/self/net/fib_trie".as_ptr(), libc::O_RDONLY);
                let mut buf = [0u8; 8192];
                let n = libc::read(f, buf.as_mut_ptr().cast(), buf.len());
                let n = usize::try_from(n).unwrap_or(0);
                let up = buf[..n].windows(9).any(|w| w == b"127.0.0.1");
                libc::_exit(if up { 0 } else { 1 });
            }
        }
        let mut status = 0;
        // SAFETY: reaping the child forked above.
        assert_eq!(unsafe { libc::waitpid(pid, &raw mut status, 0) }, pid);
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0
    }

    #[test]
    #[ignore = "T2c: netadmin: creates user and network namespaces and brings loopback up"]
    fn root_brings_up_loopback_itself_and_in_a_user_namespace() {
        assert_eq!(probe_network(false), NetMode::Clone);
        assert_eq!(probe_network(true), NetMode::Clone);
        let ns = NetNs::create(true).unwrap();
        ns.check_join().unwrap();
        assert!(loopback_is_up_in(&ns));
        let plain = NetNs::create(false).unwrap();
        assert!(loopback_is_up_in(&plain));
    }

    #[test]
    fn a_symlinked_sandbox_or_runs_directory_is_refused() {
        let state = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let sandbox = state.path().join("sandbox");
        std::os::unix::fs::symlink(elsewhere.path(), &sandbox).unwrap();
        assert!(Runner::new(&sandbox, None).is_err());
        assert!(Sandbox::start_inner(None, &sandbox, None).is_err());

        let real = state.path().join("real");
        fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), real.join("runs")).unwrap();
        assert!(Runner::new(&real, None).is_err());
        assert_eq!(fs::read_dir(elsewhere.path()).unwrap().count(), 0);
    }

    #[test]
    fn clone_flags_follow_isolation_and_network() {
        let mut spec = RunSpec::new("/r", vec!["sh".into()]);
        let full = clone_flags(&spec);
        for ns in [
            libc::CLONE_NEWNS,
            libc::CLONE_NEWPID,
            libc::CLONE_NEWUTS,
            libc::CLONE_NEWIPC,
        ] {
            assert_ne!(full & ns, 0);
        }
        assert_eq!(full & libc::CLONE_NEWNET, 0);
        assert_eq!(full & 0xff, libc::SIGCHLD);
        spec.network = Network::None;
        assert_ne!(clone_flags(&spec) & libc::CLONE_NEWNET, 0);
        spec.network = Network::Host;
        spec.isolation = Isolation::Direct;
        assert_eq!(clone_flags(&spec), libc::SIGCHLD);
    }

    #[test]
    fn failure_records_are_read_back() {
        let (r, w) = rustix::pipe::pipe_with(PipeFlags::CLOEXEC).unwrap();
        drop(w);
        assert_eq!(read_failure(&r).unwrap(), None);

        let (r, w) = rustix::pipe::pipe_with(PipeFlags::CLOEXEC).unwrap();
        let rec = child::encode_failure(Step::SecretOwner, 2, libc::EINVAL);
        rustix::io::write(&w, &rec).unwrap();
        drop(w);
        assert_eq!(
            read_failure(&r).unwrap(),
            Some((Step::SecretOwner, 2, libc::EINVAL))
        );

        let (r, w) = rustix::pipe::pipe_with(PipeFlags::CLOEXEC).unwrap();
        rustix::io::write(&w, &rec[..3]).unwrap();
        drop(w);
        assert!(read_failure(&r).is_err());
    }

    /// A switch to the `RUN`'s user refused with `EINVAL` names the ids the
    /// user namespace does not map, and still ran no command; another
    /// errno, or another step, stays as it was.
    #[test]
    fn a_user_switch_refused_with_einval_names_the_ids() {
        let user = RunUser {
            uid: Uid::from_raw(1000),
            gid: Gid::from_raw(1000),
            groups: vec![Gid::from_raw(27)],
            user_ns: true,
        };
        let setup = |step, errno| Error::Setup {
            step,
            source: io::Error::from_raw_os_error(errno),
        };
        let e = unmapped_user(setup(Step::Gid, libc::EINVAL), &user);
        assert!(e.ran_no_command());
        assert_eq!(
            e.to_string(),
            "RUN sandbox: set the group id for uid 1000, gid 1000, groups 27: an id this user \
             namespace does not map"
        );
        let source = std::error::Error::source(&e).and_then(|s| s.downcast_ref());
        assert_eq!(
            source,
            Some(&rnk_rootfs::Unmapped::user(Step::Gid, 1000, 1000, &[27]))
        );
        for step in [Step::Groups, Step::Uid] {
            let e = unmapped_user(setup(step, libc::EINVAL), &user);
            assert!(matches!(e, Error::Unmapped(_)), "{step}");
        }
        for (step, errno) in [
            (Step::Uid, libc::EPERM),
            (Step::Workdir, libc::EINVAL),
            // The dump flag's `prctl` switches no id.
            (Step::Dumpable, libc::EINVAL),
        ] {
            let e = unmapped_user(setup(step, errno), &user);
            assert!(matches!(e, Error::Setup { .. }), "{step}");
        }
    }

    /// An `EINVAL` that an unmapped id does not explain stays the step's
    /// error: in the initial user namespace every id but `-1` maps, and
    /// `setgroups` refuses more groups than the kernel's `NGROUPS_MAX`
    /// with it too.
    #[test]
    fn a_user_switch_refused_with_einval_for_another_cause_names_no_id() {
        let setup = |step| Error::Setup {
            step,
            source: io::Error::from_raw_os_error(libc::EINVAL),
        };
        let initial = RunUser {
            uid: Uid::from_raw(1000),
            gid: Gid::from_raw(1000),
            groups: vec![Gid::from_raw(27)],
            user_ns: false,
        };
        for step in [Step::Groups, Step::Gid, Step::Uid] {
            let e = unmapped_user(setup(step), &initial);
            assert!(matches!(e, Error::Setup { .. }), "{step}");
        }
        let many = |n: u32| RunUser {
            uid: Uid::from_raw(1000),
            gid: Gid::from_raw(1000),
            groups: (0..n).map(Gid::from_raw).collect(),
            user_ns: true,
        };
        let limit = u32::try_from(NGROUPS_MAX).unwrap();
        for step in [Step::Groups, Step::Gid, Step::Uid] {
            let e = unmapped_user(setup(step), &many(limit + 1));
            assert!(matches!(e, Error::Setup { .. }), "{step}");
            // The control: at the kernel's limit, the ids are the cause.
            let e = unmapped_user(setup(step), &many(limit));
            assert!(matches!(e, Error::Unmapped(_)), "{step}");
        }
        // At most eight groups are named, the rest counted.
        let e = unmapped_user(setup(Step::Groups), &many(limit));
        assert!(
            e.to_string().ends_with(
                "groups 0 1 2 3 4 5 6 7 and 65528 more: an id this user namespace does not map"
            ),
            "{e}"
        );
    }

    /// A secret file's `chown` refused with `EINVAL` in a user namespace
    /// names the file the record numbers, by its first target, and its
    /// owner, and still ran no command; another errno, step or namespace,
    /// or an item that numbers no file, stays as it was.
    #[test]
    fn a_secret_s_owner_refused_with_einval_names_the_file() {
        let file = |name: &std::ffi::CStr,
                    uid: Option<u32>,
                    gid: Option<u32>,
                    targets: &[&std::ffi::CStr]| ChildSecret {
            name: name.into(),
            data: std::sync::Arc::from(&b"s"[..]),
            uid: uid.map(Uid::from_raw),
            gid: gid.map(Gid::from_raw),
            mode: Mode::RUSR,
            targets: targets.iter().map(|&t| t.into()).collect(),
        };
        let owners = secret_owners(Some(&ChildSecrets {
            dir: c"/scratch".into(),
            options: c"size=16k".into(),
            files: vec![
                file(c"0", Some(0), Some(0), &[c"/run/secrets/a", c"/a2"]),
                file(c"1", Some(3_000_000_000), None, &[c"/b"]),
            ],
        }));
        assert!(secret_owners(None).is_empty());
        let setup = |step, errno| Error::Setup {
            step,
            source: io::Error::from_raw_os_error(errno),
        };
        let e = unmapped_secret(setup(Step::SecretOwner, libc::EINVAL), 1, &owners, true);
        assert!(e.ran_no_command());
        assert_eq!(
            e.to_string(),
            "RUN sandbox: chown /b to uid 3000000000: an id this user namespace does not map"
        );
        let e = unmapped_secret(setup(Step::SecretOwner, libc::EINVAL), 0, &owners, true);
        let source = std::error::Error::source(&e).and_then(|s| s.downcast_ref());
        assert_eq!(
            source,
            Some(&rnk_rootfs::Unmapped::chown(
                Path::new("/run/secrets/a"),
                Some(0),
                Some(0)
            ))
        );
        for (step, errno, item, user_ns) in [
            (Step::SecretOwner, libc::EPERM, 1, true),
            (Step::Secret, libc::EINVAL, 1, true),
            (Step::SecretOwner, libc::EINVAL, 1, false),
            (Step::SecretOwner, libc::EINVAL, 2, true),
            (Step::SecretOwner, libc::EINVAL, u16::MAX, true),
        ] {
            let e = unmapped_secret(setup(step, errno), item, &owners, user_ns);
            assert!(matches!(e, Error::Setup { .. }), "{step} {errno} {item}");
        }
    }

    /// A `RUN`'s end: its status, or the child's record from before
    /// `execve`. An init that ended before it wrote the command's status
    /// (`unreported`) never passes its own status off as the command's: a
    /// signal is `SetupKilled`, naming the profile only for `SIGSYS` under
    /// one, which wins over a record of `Step::Init`, which its end may
    /// cause, and loses to any other, which is more precise; a code is
    /// `InitExited`, 0 a status that could not be read (the init exits 0
    /// only after its status write), which every record wins over.
    #[test]
    fn a_runs_end_is_its_status_its_record_or_its_inits_end() {
        let exited = |code: i32| ExitStatus::from_raw(code << 8);
        let killed_by = ExitStatus::from_raw;
        let end = |status, unreported, failure, profile| {
            ending(status, unreported, failure, profile, "prog")
        };
        // The command's own status, as the init wrote it.
        assert_eq!(end(exited(3), false, None, true).unwrap().code(), Some(3));
        for code in [125, 127] {
            assert_eq!(
                end(exited(code), false, None, true).unwrap().code(),
                Some(code)
            );
        }
        let sigsys = killed_by(libc::SIGSYS);
        assert_eq!(
            end(sigsys, false, None, true).unwrap().signal(),
            Some(libc::SIGSYS)
        );
        for (status, failure, profile, seccomp) in [
            (sigsys, None, true, true),
            (sigsys, None, false, false),
            (killed_by(libc::SIGKILL), None, true, false),
            (sigsys, Some((Step::Init, libc::EPIPE)), true, true),
            (sigsys, Some((Step::Init, libc::EPERM)), true, true),
        ] {
            let r = end(status, true, failure, profile);
            assert!(
                matches!(r, Err(Error::SetupKilled { signal, seccomp: s })
                    if Some(signal) == status.signal() && s == seccomp),
                "{status:?} {failure:?} {profile}: {r:?}"
            );
        }
        // The init's own code: `_exit(125)` in its wait or where its go can
        // be neither sent nor ended, `_exit(127)` with its record refused, or
        // any other.
        for code in [125, 127, 1] {
            let r = end(exited(code), true, None, true);
            assert!(
                matches!(r, Err(Error::InitExited { code: c }) if c == code),
                "{code}: {r:?}"
            );
        }
        let r = end(exited(0), true, None, true);
        assert!(
            matches!(&r, Err(Error::Sys { op: STATUS_OP, source })
                if source.kind() == io::ErrorKind::InvalidData),
            "{r:?}"
        );
        for (status, unreported) in [
            (exited(3), false),
            (sigsys, true),
            (exited(127), true),
            (exited(0), true),
        ] {
            let r = end(
                status,
                unreported,
                Some((Step::Seccomp, libc::EINVAL)),
                true,
            );
            assert!(
                matches!(&r, Err(Error::Setup { step: Step::Seccomp, source })
                    if source.raw_os_error() == Some(libc::EINVAL)),
                "{status:?}: {r:?}"
            );
            let r = end(status, unreported, Some((Step::Exec, libc::ENOENT)), true);
            assert!(
                matches!(&r, Err(Error::Exec { program, .. }) if program == "prog"),
                "{status:?}: {r:?}"
            );
        }
        for (status, unreported) in [(exited(3), false), (exited(127), true), (exited(0), true)] {
            let r = end(status, unreported, Some((Step::Init, libc::EPIPE)), true);
            assert!(
                matches!(
                    r,
                    Err(Error::Setup {
                        step: Step::Init,
                        ..
                    })
                ),
                "{status:?}: {r:?}"
            );
        }
    }

    /// The init's status record: none (it died or failed before writing),
    /// a wait status kept bit for bit (an exit code, a signal), or a
    /// truncated one, which is an error.
    #[test]
    fn the_inits_status_records_are_read_back() {
        let (r, w) = rustix::pipe::pipe_with(PipeFlags::CLOEXEC).unwrap();
        drop(w);
        assert_eq!(read_status(&r).unwrap(), None);
        for raw in [7 << 8, libc::SIGTERM, libc::SIGKILL | 0x80] {
            let (r, w) = rustix::pipe::pipe_with(PipeFlags::CLOEXEC).unwrap();
            rustix::io::write(&w, &raw.to_le_bytes()).unwrap();
            drop(w);
            let got = read_status(&r).unwrap().unwrap();
            assert_eq!(got, raw);
            let status = ExitStatus::from_raw(got);
            assert_eq!(
                (status.code(), status.signal()),
                (
                    ExitStatus::from_raw(raw).code(),
                    ExitStatus::from_raw(raw).signal()
                )
            );
        }
        assert_eq!(ExitStatus::from_raw(7 << 8).code(), Some(7));
        assert_eq!(
            ExitStatus::from_raw(libc::SIGTERM).signal(),
            Some(libc::SIGTERM)
        );
        let (r, w) = rustix::pipe::pipe_with(PipeFlags::CLOEXEC).unwrap();
        rustix::io::write(&w, &[1, 2]).unwrap();
        drop(w);
        assert!(read_status(&r).is_err());
        // A stop is no end: refused.
        let (r, w) = rustix::pipe::pipe_with(PipeFlags::CLOEXEC).unwrap();
        let stopped: i32 = (libc::SIGTRAP << 8) | 0x7f;
        rustix::io::write(&w, &stopped.to_le_bytes()).unwrap();
        drop(w);
        assert!(read_status(&r).is_err());
    }

    /// How [`execute`] reads a full-sandbox `RUN`'s end, with a child forked
    /// here standing in for the init: it ends as the wait status `end` says
    /// (an exit code, or a signal it sends itself), leaving `record` on the
    /// error pipe and `written` on the status pipe. With `cancel`, the
    /// `RUN` is cancelled once the child has exited, so its end stays.
    fn full_runs_end(
        end: i32,
        record: Option<(Step, i32)>,
        written: &[u8],
        cancel: bool,
    ) -> Result<ExitStatus> {
        let pipe = || rustix::pipe::pipe_with(PipeFlags::CLOEXEC).unwrap();
        let (failures, err_w) = pipe();
        if let Some((step, errno)) = record {
            rustix::io::write(&err_w, &child::encode_failure(step, 0, errno)).unwrap();
        }
        drop(err_w);
        let (status, status_w) = pipe();
        rustix::io::write(&status_w, written).unwrap();
        drop(status_w);
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let runner = Runner::new(&root.join("sandbox"), Some(RootEntry::PivotRoot)).unwrap();
        let spec = RunSpec::new(&root, vec!["/prog".into()]);
        let user = User {
            uid: 0,
            gid: 0,
            supplementary_gids: Vec::new(),
            home: "/".into(),
        };
        let job = runner
            .job(
                &spec,
                &user,
                Vec::new(),
                None,
                Default::default(),
                RootEntry::PivotRoot,
                None,
            )
            .unwrap();
        // SAFETY: the child only makes async-signal-safe calls and ends.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            // SAFETY: as above.
            unsafe {
                if libc::WIFSIGNALED(end) {
                    libc::kill(libc::getpid(), libc::WTERMSIG(end));
                }
                libc::_exit(libc::WEXITSTATUS(end));
            }
        }
        let pid = Pid::from_raw(pid).unwrap();
        let running = Running::default();
        let spawn = |_| {
            wait_exited(pid).unwrap();
            if cancel {
                running.cancel();
            }
            Ok(Spawned {
                pid,
                failures,
                status: Some(status),
                netns: None,
            })
        };
        execute(job, spawn, "prog", Some(&running), false, None).map(|(s, _)| s)
    }

    /// An init that exited 0 has written the command's status, unless that
    /// write failed: without it, the `RUN` fails as a status it could not
    /// read, after a cancel and any failure record, which are more precise
    /// and win. Another code is `InitExited`.
    #[test]
    fn a_record_or_a_cancel_wins_over_an_init_that_exited_0_without_the_status() {
        let exited = |code: i32| code << 8;
        let r = full_runs_end(exited(0), None, &exited(3).to_le_bytes(), false);
        assert_eq!(r.unwrap().code(), Some(3));
        let r = full_runs_end(exited(0), None, &[], false);
        assert!(
            matches!(&r, Err(Error::Sys { op: STATUS_OP, source })
                if source.kind() == io::ErrorKind::InvalidData),
            "{r:?}"
        );
        let r = full_runs_end(exited(0), Some((Step::Seccomp, libc::EINVAL)), &[], false);
        assert!(
            matches!(&r, Err(Error::Setup { step: Step::Seccomp, source })
                if source.raw_os_error() == Some(libc::EINVAL)),
            "{r:?}"
        );
        let r = full_runs_end(exited(0), Some((Step::Exec, libc::ENOENT)), &[], false);
        assert!(
            matches!(&r, Err(Error::Exec { program, source })
                if program == "prog" && source.raw_os_error() == Some(libc::ENOENT)),
            "{r:?}"
        );
        let r = full_runs_end(exited(0), None, &[], true);
        assert!(matches!(r, Err(Error::Cancelled)), "{r:?}");
        let r = full_runs_end(exited(125), None, &[], false);
        assert!(matches!(r, Err(Error::InitExited { code: 125 })), "{r:?}");
    }

    /// Every event's fields, one `name=value ` string per event, while it
    /// is the default.
    struct Logged(std::sync::Arc<Mutex<Vec<String>>>);

    impl tracing::Subscriber for Logged {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            struct Fields(String);
            impl tracing::field::Visit for Fields {
                fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
                    self.0.push_str(&format!("{}={v:?} ", f.name()));
                }
            }
            let mut fields = Fields(String::new());
            event.record(&mut fields);
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(fields.0);
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// A status record that cannot be read fails the `RUN` with that read's
    /// error, a cancel and a failure record included; the record, the
    /// init's end (here its signal) and the cancel are logged. The only
    /// test here that reaches that log, so no other can cache it as
    /// unwanted.
    #[test]
    fn an_unreadable_status_is_the_error_and_the_rest_is_logged() {
        let seen = std::sync::Arc::default();
        let dispatch = tracing::Dispatch::new(Logged(std::sync::Arc::clone(&seen)));
        let record = (Step::MountProc, libc::EPERM);
        let r = tracing::dispatcher::with_default(&dispatch, || {
            full_runs_end(libc::SIGKILL, Some(record), &[1, 2], true)
        });
        assert!(
            matches!(&r, Err(Error::Sys { op: STATUS_OP, source })
                if source.kind() == io::ErrorKind::UnexpectedEof),
            "{r:?}"
        );
        let want = [
            format!(
                "record={:?}",
                Ok::<_, Error>(Some((record.0, 0u16, record.1)))
            ),
            format!("init={}", ExitStatus::from_raw(libc::SIGKILL)),
            "cancelled=true".to_owned(),
        ];
        let seen = seen.lock().unwrap_or_else(PoisonError::into_inner);
        assert!(
            seen.iter()
                .any(|e| want.iter().all(|w| e.contains(w.as_str()))),
            "{want:?} in {seen:?}"
        );
    }

    #[test]
    fn stdio_descriptors_are_moved_above_two() {
        let mut keep = Vec::new();
        let (r, _w) = rustix::pipe::pipe_with(PipeFlags::CLOEXEC).unwrap();
        let s = child_stdio(Stdio::Fd(r), 0, &mut keep).unwrap();
        let ChildStdio::Fd(fd) = s else {
            panic!("{s:?}")
        };
        assert!(fd >= 3);
        assert_eq!(keep.len(), 1);
        assert_eq!(
            child_stdio(Stdio::Null, 0, &mut keep).unwrap(),
            ChildStdio::Null
        );
    }

    /// A directory or an `O_PATH` descriptor is refused as a `RUN` stream:
    /// it would cross `execve` as a way out of the root.
    #[test]
    fn a_directory_or_path_descriptor_is_no_stream() {
        let mut keep = Vec::new();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        fs::write(&file, b"x").unwrap();
        let opened = |p: &Path, flags: OFlags| {
            rustix::fs::open(p, flags | OFlags::CLOEXEC, Mode::empty()).unwrap()
        };
        for (what, fd) in [
            (
                "a directory",
                opened(dir.path(), OFlags::RDONLY | OFlags::DIRECTORY),
            ),
            ("an O_PATH directory", opened(dir.path(), OFlags::PATH)),
            ("an O_PATH file", opened(&file, OFlags::PATH)),
        ] {
            let r = child_stdio(Stdio::Fd(fd), 1, &mut keep);
            assert!(
                matches!(&r, Err(Error::InvalidSpec(m)) if m.contains("stdout")),
                "{what}: {r:?}"
            );
        }
        assert!(keep.is_empty());
        let ok = child_stdio(Stdio::Fd(opened(&file, OFlags::RDONLY)), 1, &mut keep);
        assert!(matches!(ok, Ok(ChildStdio::Fd(_))), "{ok:?}");
        // The test process's own streams are no directory.
        assert_eq!(
            child_stdio(Stdio::Inherit, 2, &mut keep).unwrap(),
            ChildStdio::Inherit
        );
    }

    #[test]
    fn usage_is_converted_without_negative_values() {
        let t = libc::timeval {
            tv_sec: 2,
            tv_usec: 500,
        };
        assert_eq!(timeval_ns(t), 2_000_500_000);
        assert_eq!(count(-1i64), 0);
    }

    #[test]
    fn scratch_files_ignore_the_umask() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("hosts");
        let old = rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o077));
        let r = write_scratch(&p, b"x");
        rustix::process::umask(old);
        r.unwrap();
        let mode = fs::metadata(&p).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o644);
        assert!(write_scratch(&p, b"y").is_err(), "never overwrites");
    }

    /// A `Direct` run of a program that does not exist, under a fresh
    /// empty directory: whatever the privileges of the test, the child fails
    /// before executing anything.
    fn direct(missing: &Path, workdir: &Path) -> RunSpec {
        let mut spec = RunSpec::new("/", vec![missing.into()]);
        spec.isolation = Isolation::Direct;
        spec.workdir = workdir.into();
        spec
    }

    /// A real clone through the spawner thread, without namespaces: the
    /// failure record, the process-group kill and the reaping. Nothing is
    /// executed: a missing `WORKDIR` fails before the `USER` switch, and
    /// with `/` a normal user fails at `setgroups`, root at `execve`.
    #[test]
    fn direct_children_report_their_failing_step() {
        let dir = tempfile::tempdir().unwrap();
        let empty = tempfile::tempdir().unwrap();
        let empty = empty.path().canonicalize().unwrap();
        let missing = empty.join("missing");
        let sandbox = Sandbox::start_inner(None, dir.path(), None).unwrap();
        match sandbox.run(direct(&missing, &missing)) {
            Err(Error::Setup {
                step: Step::Workdir,
                source,
            }) => assert_eq!(source.raw_os_error(), Some(libc::ENOENT)),
            r => panic!("{r:?}"),
        }
        match sandbox.run(direct(&missing, Path::new("/"))) {
            Err(Error::Setup {
                step: Step::Groups,
                source,
            }) => assert_eq!(source.raw_os_error(), Some(libc::EPERM)),
            Err(Error::Exec { program, source }) => {
                assert_eq!(Path::new(&program), missing);
                assert_eq!(source.raw_os_error(), Some(libc::ENOENT));
            }
            r => panic!("{r:?}"),
        }
        // No probe result: the full sandbox is refused before any change.
        let root = tempfile::tempdir().unwrap();
        let spec = RunSpec::new(root.path().canonicalize().unwrap(), vec!["sh".into()]);
        assert!(matches!(sandbox.run(spec), Err(Error::Privilege(_))));
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
        drop(sandbox);
        assert_eq!(fs::read_dir(dir.path().join("runs")).unwrap().count(), 0);
    }

    /// A root for `Direct` other than `/`, with an image's `/etc/hosts` and
    /// old `/etc` times; returns the hosts inode and those times.
    fn direct_root(dir: &Path) -> (u64, rnk_rootfs::FileTime) {
        use std::os::unix::fs::MetadataExt as _;
        fs::create_dir_all(dir.join("etc")).unwrap();
        fs::write(dir.join("etc/hosts"), b"image").unwrap();
        let root = RootPath::open(dir).unwrap();
        let old = rnk_rootfs::FileTime::from_secs(1_000_000);
        root.set_meta(
            "/etc",
            &rnk_rootfs::Meta {
                atime: Some(old),
                mtime: Some(old),
                ..rnk_rootfs::Meta::default()
            },
        )
        .unwrap();
        let ino = fs::metadata(dir.join("etc/hosts")).unwrap().ino();
        (ino, old)
    }

    /// What a `Direct` root holds again after the `RUN`: the image's
    /// `/etc/hosts` (same inode), nothing placed, `/etc`'s times.
    fn assert_direct_root_restored(dir: &Path, ino: u64, old: rnk_rootfs::FileTime) {
        use std::os::unix::fs::MetadataExt as _;
        assert_eq!(fs::metadata(dir.join("etc/hosts")).unwrap().ino(), ino);
        assert_eq!(fs::read(dir.join("etc/hosts")).unwrap(), b"image");
        let mut etc: Vec<_> = fs::read_dir(dir.join("etc"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        etc.sort();
        assert_eq!(etc, ["hosts"]);
        assert!(!dir.join("dev").exists(), "the /dev stub is removed");
        let root = RootPath::open(dir).unwrap();
        assert_eq!(root.statx("/etc").unwrap().mtime, old);
    }

    /// `Direct` on a root other than `/`: as a normal user the child stops at
    /// `chroot` (as root, at the missing program); either way the files
    /// placed for it are gone and the image's are back.
    #[test]
    fn direct_on_a_workdir_places_and_restores_its_files() {
        let dir = tempfile::tempdir().unwrap();
        let wd = tempfile::tempdir().unwrap();
        let wd = wd.path().canonicalize().unwrap();
        let (ino, old) = direct_root(&wd);
        let sandbox =
            Sandbox::start_with_root(None, dir.path(), &wd, &RootPath::open(&wd).unwrap()).unwrap();
        let mut spec = RunSpec::new(&wd, vec!["/missing".into()]);
        spec.isolation = Isolation::Direct;
        match sandbox.run(spec) {
            Err(Error::Setup {
                step: Step::Chroot,
                source,
            }) => assert_eq!(source.raw_os_error(), Some(libc::EPERM)),
            Err(Error::Exec { source, .. }) => {
                assert_eq!(source.raw_os_error(), Some(libc::ENOENT));
            }
            r => panic!("{r:?}"),
        }
        assert_direct_root_restored(&wd, ino, old);
        drop(sandbox);
        assert_eq!(fs::read_dir(dir.path().join("runs")).unwrap().count(), 0);
        assert!(
            Sandbox::start_with_root(
                None,
                dir.path(),
                Path::new("/"),
                &RootPath::open("/").unwrap()
            )
            .is_err()
        );
    }

    /// Where the probe found the full sandbox possible, a bare `chroot` is
    /// refused before anything is placed; so is a root that is the host's
    /// `/etc` or `/dev` by identity.
    #[test]
    fn direct_off_slash_is_refused_where_it_must_not_run() {
        let dir = tempfile::tempdir().unwrap();
        let wd = tempfile::tempdir().unwrap();
        let wd = wd.path().canonicalize().unwrap();
        let (ino, old) = direct_root(&wd);
        let runner = Runner::new(dir.path(), Some(RootEntry::PivotRoot)).unwrap();
        let mut spec = RunSpec::new(&wd, vec!["/missing".into()]);
        spec.isolation = Isolation::Direct;
        let r = runner.run(spec, None, |_| panic!("nothing is cloned"));
        assert!(matches!(r, Err(Error::InvalidSpec(_))), "{r:?}");
        assert_direct_root_restored(&wd, ino, old);

        let runner = Runner::new(dir.path(), None).unwrap();
        for host in ["/etc", "/dev"] {
            let mut spec = RunSpec::new(host, vec!["/missing".into()]);
            spec.isolation = Isolation::Direct;
            let r = runner.run(spec, None, |_| panic!("nothing is cloned"));
            assert!(matches!(r, Err(Error::InvalidSpec(_))), "{host}: {r:?}");
        }
        assert!(
            Sandbox::start_with_root(
                None,
                dir.path(),
                Path::new("/etc"),
                &RootPath::open("/etc").unwrap()
            )
            .is_err()
        );
        assert_eq!(fs::read_dir(dir.path().join("runs")).unwrap().count(), 0);
    }

    #[test]
    fn a_shared_root_swapped_since_the_start_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let parent = tempfile::tempdir().unwrap();
        let wd = parent.path().canonicalize().unwrap().join("w");
        fs::create_dir(&wd).unwrap();
        let id = root_id(&RootPath::open(&wd).unwrap()).unwrap();
        fs::rename(&wd, parent.path().join("moved")).unwrap();
        fs::create_dir(&wd).unwrap();
        // `Direct` where nothing can mount, `Full` where the probe passed.
        for (isolation, entry) in [
            (Isolation::Direct, None),
            (Isolation::Full, Some(RootEntry::PivotRoot)),
        ] {
            let mut runner = Runner::new(dir.path(), entry).unwrap();
            runner.shared_root = Some((wd.clone(), id));
            let mut spec = RunSpec::new(&wd, vec!["/missing".into()]);
            spec.isolation = isolation;
            let r = runner.run(spec, None, |_| panic!("nothing is cloned"));
            let Err(Error::InvalidSpec(why)) = r else {
                panic!("{isolation:?}: {r:?}");
            };
            assert!(why.contains("no longer"), "{why}");
        }
        // A RUN on another path than the shared root is refused too.
        let mut runner = Runner::new(dir.path(), None).unwrap();
        runner.shared_root = Some((wd.clone(), id));
        let other = parent.path().canonicalize().unwrap().join("moved");
        let mut spec = RunSpec::new(&other, vec!["/missing".into()]);
        spec.isolation = Isolation::Direct;
        let r = runner.run(spec, None, |_| panic!("nothing is cloned"));
        assert!(
            matches!(&r, Err(Error::InvalidSpec(w)) if w.contains("shared root is")),
            "{r:?}"
        );
        // And a start on a path that is not the checked directory.
        let e = Sandbox::start_with_root(None, dir.path(), &wd, &RootPath::open(&other).unwrap());
        assert!(
            matches!(&e, Err(Error::InvalidSpec(w)) if w.contains("caller checked")),
            "{:?}",
            e.err()
        );
        assert_eq!(fs::read_dir(dir.path().join("runs")).unwrap().count(), 0);
    }

    /// The root the caller holds (`RunSpec::expect_root`, ADR-0059): a path
    /// swapped since for another directory is refused before anything is
    /// placed or cloned, in every isolation.
    #[test]
    fn a_root_swapped_since_the_caller_opened_it_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let parent = tempfile::tempdir().unwrap();
        let wd = parent.path().canonicalize().unwrap().join("w");
        fs::create_dir(&wd).unwrap();
        let held = RootPath::open(&wd).unwrap();
        fs::rename(&wd, parent.path().join("moved")).unwrap();
        fs::create_dir(&wd).unwrap();
        for (isolation, entry) in [
            (Isolation::Direct, None),
            (Isolation::Full, Some(RootEntry::PivotRoot)),
        ] {
            let runner = Runner::new(dir.path(), entry).unwrap();
            let mut spec = RunSpec::new(&wd, vec!["/missing".into()]);
            spec.isolation = isolation;
            spec.expect_root(&held).unwrap();
            let r = runner.run(spec, None, |_| panic!("nothing is cloned"));
            let Err(Error::InvalidSpec(why)) = r else {
                panic!("{isolation:?}: {r:?}");
            };
            assert!(why.contains("no longer the directory"), "{why}");
        }
        assert_eq!(fs::read_dir(dir.path().join("runs")).unwrap().count(), 0);
    }

    /// A real `Direct` `RUN` in a `chroot`, as root in the VM: the shell and
    /// its libraries are copied from the VM into a fresh root, which then
    /// sees ranikust's `hosts` and `resolv.conf`, `/dev` nodes, no `/proc`,
    /// and the `RLIMIT_NOFILE` and `RLIMIT_CORE` given back; after it, the
    /// root is as before.
    #[test]
    #[ignore = "T2: chroot and mknod as real root; run in the test VM only"]
    fn direct_runs_in_a_chroot_with_placed_files_as_root() {
        let dir = tempfile::tempdir().unwrap();
        let wd = tempfile::tempdir().unwrap();
        let wd = wd.path().canonicalize().unwrap();
        let (ino, old) = direct_root(&wd);
        copy_with_libraries(Path::new("/bin/sh"), &wd);
        let mut sandbox =
            Sandbox::start_with_root(None, dir.path(), &wd, &RootPath::open(&wd).unwrap()).unwrap();
        let hard = crate::NofileLimit::current().hard;
        sandbox.set_nofile_limit(crate::NofileLimit {
            soft: Some(777),
            hard,
        });
        // An unlimited core size (raised as root: the VM's hard limit may be
        // 0) that `forbid_core_dumps` cuts to 0 here and the `RUN` gets back.
        rustix::process::setrlimit(
            rustix::process::Resource::Core,
            rustix::process::Rlimit {
                current: None,
                maximum: None,
            },
        )
        .unwrap();
        crate::forbid_core_dumps().unwrap();
        assert_eq!(
            rustix::process::getrlimit(rustix::process::Resource::Core).current,
            Some(0)
        );
        let script = "exec >/out.txt; \
            while read -r l; do echo \"hosts:$l\"; done </etc/hosts; \
            [ -f /etc/resolv.conf ] && echo resolv; \
            [ -c /dev/null ] && echo null; [ -c /dev/urandom ] && echo urandom; \
            [ -d /proc/self ] || echo noproc; \
            echo \"nofile:$(ulimit -n)\"; echo \"core:$(ulimit -c)\"; \
            echo x >/dev/null && echo devnull-ok";
        let mut spec = RunSpec::new(&wd, vec!["/bin/sh".into(), "-c".into(), script.into()]);
        spec.isolation = Isolation::Direct;
        let status = sandbox.run(spec).unwrap();
        assert!(status.success(), "{status:?}");
        let out = fs::read_to_string(wd.join("out.txt")).unwrap();
        for line in [
            "resolv",
            "null",
            "urandom",
            "noproc",
            "nofile:777",
            "devnull-ok",
        ] {
            assert!(out.lines().any(|l| l == line), "{line}: {out}");
        }
        assert!(out.lines().any(|l| l == "core:unlimited"), "{out}");
        assert!(
            out.contains("hosts:") && !out.contains("hosts:image"),
            "{out}"
        );
        fs::remove_file(wd.join("out.txt")).unwrap();
        fs::remove_dir_all(wd.join("bin")).unwrap();
        for top in ["lib", "lib64", "usr"] {
            let _ = fs::remove_dir_all(wd.join(top));
        }
        assert_direct_root_restored(&wd, ino, old);
    }

    /// Copies `bin` into `root` at the same path, with the libraries `ldd`
    /// lists for it.
    fn copy_with_libraries(bin: &Path, root: &Path) {
        let ldd = std::process::Command::new("ldd").arg(bin).output().unwrap();
        let text = String::from_utf8(ldd.stdout).unwrap();
        let libs = text
            .split_whitespace()
            .filter(|w| w.starts_with('/'))
            .map(PathBuf::from);
        for p in std::iter::once(bin.to_path_buf()).chain(libs) {
            let real = p.canonicalize().unwrap();
            let dst = root.join(p.strip_prefix("/").unwrap());
            fs::create_dir_all(dst.parent().unwrap()).unwrap();
            fs::copy(&real, &dst).unwrap();
        }
    }

    /// A cancel kills a registered child, and the exit is seen before the
    /// reap, so the pid is still the child's when the kill comes.
    #[test]
    fn a_cancel_kills_running_children_and_later_ones() {
        let running = Running::default();
        let child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = Pid::from_raw(i32::try_from(child.id()).unwrap()).unwrap();
        running.add(pid, Isolation::Direct, None);
        let start = std::time::Instant::now();
        std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(100));
                running.cancel();
            });
            wait_exited(pid).unwrap();
        });
        assert!(running.remove(pid), "the cancel is reported");
        let (status, _) = reap(pid).unwrap();
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
        let mut child = child;
        assert!(child.wait().is_err(), "already reaped");
        // After the cancel, a new child dies as soon as it is recorded.
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = Pid::from_raw(i32::try_from(child.id()).unwrap()).unwrap();
        running.add(pid, Isolation::Full, None);
        wait_exited(pid).unwrap();
        assert!(running.remove(pid));
        assert_eq!(reap(pid).unwrap().0.signal(), Some(libc::SIGKILL));
        assert!(child.wait().is_err(), "already reaped");
    }

    /// Mounts of one secret allocation, owner and mode share one tmpfs
    /// file; the tmpfs is sized by the files, not the mounts.
    #[test]
    fn one_secret_mounted_many_times_is_one_file() {
        let data: std::sync::Arc<[u8]> = vec![7u8; 1000].into();
        let file = |target: &str, mode| SecretFile {
            data: std::sync::Arc::clone(&data),
            target: target.into(),
            uid: 0,
            gid: 0,
            mode,
        };
        let many: Vec<SecretFile> = (0..1000).map(|i| file(&format!("/s{i}"), 0o400)).collect();
        let s = child_secrets(Path::new("/scratch"), &many).unwrap();
        assert_eq!(s.files.len(), 1);
        assert_eq!(s.files[0].targets.len(), 1000);
        assert_eq!(s.options.as_bytes(), b"size=16384,mode=0700");
        let s = child_secrets(
            Path::new("/scratch"),
            &[file("/a", 0o400), file("/b", 0o440)],
        )
        .unwrap();
        assert_eq!(s.files.len(), 2, "another mode, another file");
    }

    /// A secret's owner id of `u32::MAX` is `chown`'s "unchanged": none is
    /// given, so the file keeps the child's own.
    #[test]
    fn a_secret_owner_of_u32_max_is_left_unchanged() {
        let data: std::sync::Arc<[u8]> = vec![7u8; 10].into();
        let file = |uid, gid| SecretFile {
            data: std::sync::Arc::clone(&data),
            target: "/s".into(),
            uid,
            gid,
            mode: 0o400,
        };
        let s = child_secrets(
            Path::new("/scratch"),
            &[file(u32::MAX, 42), file(1000, u32::MAX)],
        )
        .unwrap();
        let owners: Vec<_> = s.files.iter().map(|f| (f.uid, f.gid)).collect();
        assert_eq!(
            owners,
            [
                (None, Some(Gid::from_raw(42))),
                (Some(Uid::from_raw(1000)), None)
            ]
        );
    }

    #[test]
    fn after_a_cancel_no_run_starts() {
        let dir = tempfile::tempdir().unwrap();
        let empty = tempfile::tempdir().unwrap();
        let missing = empty.path().canonicalize().unwrap().join("missing");
        let sandbox = Sandbox::start_inner(None, dir.path(), None).unwrap();
        sandbox.cancel();
        assert!(matches!(
            sandbox.run(direct(&missing, Path::new("/"))),
            Err(Error::Cancelled)
        ));
        drop(sandbox);
        assert_eq!(fs::read_dir(dir.path().join("runs")).unwrap().count(), 0);
    }

    /// A real `RUN` killed mid-way: root is needed for `setgroups`, so it
    /// runs as root (T2c: the VM, or a default rootful Docker run) on its own `/bin/sleep`.
    #[test]
    #[ignore = "T2c: default: a Direct RUN switches groups on the container's /"]
    fn a_cancel_kills_a_running_run() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = Sandbox::start_inner(None, dir.path(), None).unwrap();
        let mut spec = RunSpec::new("/", vec!["/bin/sleep".into(), "60".into()]);
        spec.isolation = Isolation::Direct;
        spec.workdir = "/".into();
        let start = std::time::Instant::now();
        let r = std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(500));
                sandbox.cancel();
            });
            sandbox.run(spec)
        });
        assert!(matches!(r, Err(Error::Cancelled)), "{r:?}");
        assert!(start.elapsed() < std::time::Duration::from_secs(20));
    }

    /// Processes a `Direct` `RUN` leaves, in its group, `setsid` out of it
    /// or double-forked, are gone when `run` returns (ADR-0051). Real root
    /// (the `RUN` switches to root's ids); T2c.
    #[test]
    #[ignore = "T2c: default: a Direct RUN switches groups on the container's /"]
    fn a_direct_run_leaves_no_process_behind() {
        let sandbox = Sandbox::start_direct().unwrap();
        // `sleep` under a name of this test's own: its processes, alive or
        // zombie, are told from those of tests running beside it by their
        // `comm`, which a zombie keeps.
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let sleeper = dir.path().join("rnkzsleeper");
        std::os::unix::fs::symlink("/bin/sleep", &sleeper).unwrap();
        let script = format!(
            "{s} 3001 & setsid {s} 3002 & (setsid sh -c '{s} 3003 &' &); exit 0",
            s = sleeper.display()
        );
        let mut spec = RunSpec::new("/", vec!["/bin/sh".into(), "-c".into(), script.into()]);
        spec.isolation = Isolation::Direct;
        spec.workdir = "/".into();
        let status = sandbox.run(spec).unwrap();
        assert!(status.success());
        // Neither alive nor zombies: the subreaper killed and reaped them
        // (issue mining, buildkit#5763, kaniko#1697).
        let mut left = Vec::new();
        for entry in fs::read_dir("/proc").unwrap().flatten() {
            let Ok(stat) = fs::read(entry.path().join("stat")) else {
                continue;
            };
            if stat.windows(13).any(|w| w == b"(rnkzsleeper)") {
                left.push(String::from_utf8_lossy(&stat).into_owned());
            }
        }
        assert!(left.is_empty(), "left behind: {left:?}");
    }

    /// A `#!` heredoc of a `Direct` `RUN` on `/` runs from a sealed memfd
    /// that the interpreter reads as `/dev/fd/<n>`; nothing is named in
    /// the root (ADR-0057). Also as a `USER` other than root, which then
    /// writes `/dev/stdout` and `/dev/stderr` through pipes of its own
    /// (kaniko#2928). T2c.
    #[test]
    #[ignore = "T2c: default: a Direct RUN switches groups on the container's /"]
    fn a_direct_heredoc_runs_from_an_anonymous_file() {
        use std::os::unix::fs::PermissionsExt as _;
        let sandbox = Sandbox::start_direct().unwrap();
        // In `/tmp` itself: the test's `TMPDIR` may be closed to uid 65534.
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        fs::set_permissions(dir.path(), Permissions::from_mode(0o777)).unwrap();
        for user in ["", "65534:65534"] {
            let out = dir.path().join(format!("out-{}", user.len()));
            let body = format!(
                "#!/bin/sh\necho \"$0\" > {out}\nid -u >> {out}\n\
                 echo to-stdout > /dev/stdout\necho to-stderr > /dev/stderr\n",
                out = out.display()
            );
            let mut spec = RunSpec::new("/", vec!["/dev/pipes/EOF".into()]);
            spec.isolation = Isolation::Direct;
            spec.workdir = "/".into();
            spec.user = user.into();
            spec.script = Some(body.into_bytes().into());
            let status = sandbox.run(spec).unwrap();
            assert!(status.success(), "{user:?}: {status:?}");
            let got = fs::read_to_string(&out).unwrap();
            let mut lines = got.lines();
            assert!(lines.next().unwrap().starts_with("/dev/fd/"), "{got}");
            let uid = if user.is_empty() { "0" } else { "65534" };
            assert_eq!(lines.next(), Some(uid), "{got}");
        }
        // A non-root `USER` without a script writes the streams too.
        let mut spec = RunSpec::new(
            "/",
            vec![
                "/bin/sh".into(),
                "-c".into(),
                "echo a > /dev/stdout && echo b > /dev/stderr".into(),
            ],
        );
        spec.isolation = Isolation::Direct;
        spec.workdir = "/".into();
        spec.user = "65534:65534".into();
        assert!(sandbox.run(spec).unwrap().success());
    }

    /// A `Direct` `RUN` on `/` never holds `CAP_SYS_PTRACE` (ADR-0051):
    /// the VM's root has every capability, all dropped from the bounding
    /// set but Docker's defaults. T2.
    #[test]
    #[ignore = "T2: a Direct RUN switches groups; run as root in the test VM only"]
    fn a_direct_run_on_slash_keeps_only_dockers_capabilities() {
        let sandbox = Sandbox::start_direct().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("status");
        let script = format!("grep CapBnd /proc/self/status > {}", out.display());
        let mut spec = RunSpec::new("/", vec!["/bin/sh".into(), "-c".into(), script.into()]);
        spec.isolation = Isolation::Direct;
        spec.workdir = "/".into();
        assert!(sandbox.run(spec).unwrap().success());
        let line = fs::read_to_string(&out).unwrap();
        let hex = line.split_whitespace().nth(1).unwrap();
        let bounding = u64::from_str_radix(hex, 16).unwrap();
        assert_eq!(bounding & !caps::docker_default_mask(), 0, "{line}");
    }

    /// The script of a `Direct` heredoc is sealed: nobody changes it once
    /// written, the `RUN` included.
    #[test]
    fn heredoc_scripts_are_sealed_memfds() {
        use rustix::fs::SealFlags;
        let fd = script_memfd(b"#!/bin/sh\necho hi\n").unwrap();
        assert!(fd.as_raw_fd() >= 3);
        let mut buf = [0u8; 64];
        let n = rustix::io::pread(&fd, &mut buf, 0).unwrap();
        assert_eq!(&buf[..n], b"#!/bin/sh\necho hi\n");
        let seals = rustix::fs::fcntl_get_seals(&fd).unwrap();
        assert!(
            seals
                .contains(SealFlags::SEAL | SealFlags::SHRINK | SealFlags::GROW | SealFlags::WRITE)
        );
        assert_eq!(rustix::io::pwrite(&fd, b"x", 0), Err(Errno::PERM));
        let flags = rustix::io::fcntl_getfd(&fd).unwrap();
        assert!(flags.contains(rustix::io::FdFlags::CLOEXEC));
    }

    #[test]
    fn mount_sources_must_be_what_they_claim() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        fs::create_dir(d.join("cache")).unwrap();
        fs::write(d.join("secret"), b"s").unwrap();
        std::os::unix::fs::symlink("/", d.join("link")).unwrap();
        assert!(check_source(&d.join("cache"), MountKind::Directory).is_ok());
        assert!(check_source(&d.join("secret"), MountKind::File).is_ok());
        assert!(check_source(&d.join("secret"), MountKind::Directory).is_err());
        assert!(check_source(&d.join("cache"), MountKind::File).is_err());
        // A symlink as the last component is never followed.
        assert!(check_source(&d.join("link"), MountKind::Directory).is_err());
        let m = child_mount(
            MountOp::Bind {
                source: d.join("cache"),
                target: "/root/.cache".into(),
                kind: MountKind::Directory,
                read_only: true,
            },
            false,
        )
        .unwrap();
        let ChildMount::Bind {
            source,
            dir: is_dir,
            remount,
            ..
        } = m
        else {
            panic!("{m:?}")
        };
        // A path: the child opens it in its own mount namespace.
        assert_eq!(source.as_bytes(), d.join("cache").as_os_str().as_bytes());
        assert!(is_dir);
        assert!(remount.contains(MountFlags::BIND | MountFlags::NODEV | MountFlags::RDONLY));
    }

    /// Where the probe chose an rbind of `/sys`, a `RUN` with a network
    /// namespace of its own tries a new `sysfs` first; one sharing
    /// ranikust's keeps the rbind, and a new one stays new.
    #[test]
    fn a_run_with_its_own_network_tries_a_new_sysfs_first() {
        let sysfs = |how, own_network| {
            let op = MountOp::Sysfs {
                target: "/sys".into(),
                how,
            };
            match child_mount(op, own_network).unwrap() {
                ChildMount::Sysfs {
                    rbind, fresh_first, ..
                } => (rbind, fresh_first),
                m => panic!("{m:?}"),
            }
        };
        assert_eq!(sysfs(KernelFs::Rbind, true), (true, true));
        assert_eq!(sysfs(KernelFs::Rbind, false), (true, false));
        assert_eq!(sysfs(KernelFs::Fresh, true), (false, false));
        assert_eq!(sysfs(KernelFs::Fresh, false), (false, false));
    }

    /// The fallback without `mount_setattr`: a mount point strictly below
    /// the root is found, escapes undone; the root itself, a sibling with a
    /// common prefix and mounts above it are not.
    #[test]
    fn a_mount_below_the_root_is_found_in_mountinfo() {
        let above = b"22 1 8:1 / / rw - ext4 /dev/sda1 rw\n\
            30 22 0:5 / /state/builds/1/rootfs rw - tmpfs t rw\n\
            31 22 0:6 / /state/builds/1/rootfs2 rw - tmpfs t rw\n";
        let below = b"32 30 0:7 / /state/builds/1/rootfs/a\\040b rw - tmpfs t rw\n";
        let root = b"/state/builds/1/rootfs";
        assert_eq!(submount_below(above, root), None);
        let all = [&above[..], &below[..]].concat();
        assert_eq!(
            submount_below(&all, root).as_deref(),
            Some(&b"/state/builds/1/rootfs/a b"[..])
        );
        assert_eq!(submount_below(&all, b"/state/builds/1/rootfs2"), None);
        assert_eq!(unescape_octal(b"a\\134b\\011c\\0"), b"a\\b\tc\\0");
    }

    /// A root reached through a symlinked parent is refused: mount points
    /// are compared with its path byte for byte. Its resolved path passes.
    #[test]
    fn a_root_behind_a_symlink_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let top = tmp.path().canonicalize().unwrap();
        fs::create_dir_all(top.join("real/root")).unwrap();
        std::os::unix::fs::symlink("real", top.join("link")).unwrap();
        let via = top.join("link/root");
        let real = top.join("real/root");
        // `RootPath::open` already refuses a symlink on the way (`ELOOP`);
        // the path check holds on its own too.
        assert!(RootPath::open(&via).is_err());
        let e = refuse_unresolved(&via, &RootPath::open(&real).unwrap()).unwrap_err();
        assert!(e.to_string().contains("must be given resolved"), "{e}");
        refuse_unresolved(&real, &RootPath::open(&real).unwrap()).unwrap();
        // The path now leads to another directory than the one held.
        let other = top.join("real/other");
        fs::create_dir(&other).unwrap();
        assert!(refuse_unresolved(&real, &RootPath::open(&other).unwrap()).is_err());
        refuse_unresolved(Path::new("/"), &RootPath::open("/").unwrap()).unwrap();
    }

    /// The probe of `mount_setattr` answers the same every time and never
    /// crashes where the syscall is missing.
    #[test]
    fn mount_setattr_is_probed_once() {
        let first = mount_setattr_supported();
        assert_eq!(first, mount_setattr_supported());
    }

    /// A T2 test's temporary directory and the mounts it made below it, in
    /// its private mount namespace. On drop, panics included, each mount is
    /// detached, last first, before the directory is removed; if one cannot
    /// be, the directory is kept rather than removed through the mount.
    pub(super) struct TestMounts {
        dir: Option<tempfile::TempDir>,
        targets: Vec<PathBuf>,
    }

    impl TestMounts {
        pub(super) fn new(dir: tempfile::TempDir) -> Self {
            Self {
                dir: Some(dir),
                targets: Vec::new(),
            }
        }

        pub(super) fn path(&self) -> &Path {
            self.dir
                .as_ref()
                .map_or(Path::new("/nonexistent"), |d| d.path())
        }

        /// Records a mount just made, right after it succeeded.
        pub(super) fn add(&mut self, target: PathBuf) {
            self.targets.push(target);
        }
    }

    impl Drop for TestMounts {
        fn drop(&mut self) {
            let mut detached = true;
            for t in self.targets.iter().rev() {
                if let Err(e) = rustix::mount::unmount(t, rustix::mount::UnmountFlags::DETACH) {
                    eprintln!("cannot detach {}: {e}", t.display());
                    detached = false;
                }
            }
            if let Some(dir) = self.dir.take()
                && !detached
            {
                eprintln!("left in place, not removed: {}", dir.keep().display());
            }
        }
    }

    /// T2 (root, the VM only): a mount below a full-sandbox root, holding a
    /// device node, is `nodev` in the `RUN` too, not only the root's own
    /// mount: writing to `/sub/null` fails, while the sandbox's own
    /// `/dev/null` opens. The test runs on a thread of its own in a new
    /// mount namespace, all mounts private, so nothing it mounts reaches
    /// another namespace.
    #[test]
    #[ignore = "T2: mounts and mknod as real root in a private mount namespace; run in the test VM only"]
    fn a_mount_below_the_root_is_nodev_in_the_run() {
        std::thread::spawn(|| {
            use rustix::mount::MountPropagationFlags;
            use rustix::thread::UnshareFlags;
            // SAFETY: this thread shares nothing that the unshared
            // filesystem attributes or mount namespace would break; it ends
            // with the test.
            unsafe { rustix::thread::unshare_unsafe(UnshareFlags::FS | UnshareFlags::NEWNS) }
                .expect("unshare(CLONE_FS | CLONE_NEWNS)");
            rustix::mount::mount_change(
                "/",
                MountPropagationFlags::PRIVATE | MountPropagationFlags::REC,
            )
            .expect("make every mount private");
            assert!(mount_setattr_supported(), "the VM kernel has mount_setattr");
            // Owns the directory: every mount below it is detached before
            // it is removed, on a panic too, or it is leaked.
            let mut mounts = TestMounts::new(tempfile::tempdir().unwrap());
            let top = mounts.path().canonicalize().unwrap();
            let root = top.join("root");
            fs::create_dir_all(root.join("usr")).unwrap();
            fs::create_dir(root.join("sub")).unwrap();
            for l in ["bin", "lib", "lib64", "sbin"] {
                std::os::unix::fs::symlink(format!("usr/{l}"), root.join(l)).unwrap();
            }
            rustix::mount::mount_bind("/usr", root.join("usr")).expect("bind /usr");
            mounts.add(root.join("usr"));
            // Read-only: neither the `RUN` nor a removal reaches the real
            // `/usr` through it.
            rustix::mount::mount_remount(
                root.join("usr"),
                MountFlags::BIND | MountFlags::RDONLY,
                c"",
            )
            .expect("remount the /usr bind read-only");
            rustix::mount::mount(
                "tmpfs",
                root.join("sub"),
                "tmpfs",
                MountFlags::empty(),
                None,
            )
            .expect("tmpfs below the root");
            mounts.add(root.join("sub"));
            rustix::fs::mknodat(
                rustix::fs::CWD,
                root.join("sub/null"),
                FileType::CharacterDevice,
                Mode::from_raw_mode(0o666),
                rustix::fs::makedev(1, 3),
            )
            .expect("mknod /sub/null");
            let runner = Runner::new(&top.join("sandbox"), Some(RootEntry::PivotRoot)).unwrap();
            let parent = ParentPid::new().unwrap();
            let sh = |script: &str| {
                let spec = RunSpec::new(&root, vec!["/bin/sh".into(), "-c".into(), script.into()]);
                runner
                    .run(spec, None, |job| clone_child(job, parent))
                    .unwrap()
            };
            let control = sh("echo x > /dev/null");
            let below = sh("echo x > /sub/null");
            drop(mounts);
            assert!(
                control.success(),
                "the sandbox's /dev/null opens: {control:?}"
            );
            assert!(
                !below.success(),
                "a device node on a submount opens: {below:?}"
            );
        })
        .join()
        .unwrap();
    }

    /// Red team (ADR-0060, T18): the fallback reads this process's real
    /// mountinfo: `/` has mounts below it (`/proc` at least) and is
    /// refused, naming one; a fresh directory has none and passes.
    #[test]
    fn red_team_the_submount_fallback_reads_the_real_mountinfo() {
        let r = refuse_submounts(Path::new("/"));
        assert!(
            matches!(&r, Err(Error::InvalidSpec(m)) if m.contains("mount below")),
            "{r:?}"
        );
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        refuse_submounts(&root).unwrap();
    }

    /// Red team (ADR-0060, T18): mount points with names a build can
    /// choose (spaces, tabs, newlines, backslashes, a literal `\040`, a
    /// deleted mount point) are found below a root spelled the same way;
    /// a sibling whose name only starts like the root (`root x`,
    /// `root\nx`) is not, and every mount is below `/`.
    #[test]
    fn red_team_hostile_mount_point_names_are_matched_exactly() {
        let line = |mp: &str| format!("40 22 0:9 / {mp} rw - tmpfs t rw\n");
        // (mount point as mountinfo prints it, root, mount found below it)
        type Case<'a> = (&'a str, &'a [u8], Option<&'a [u8]>);
        let cases: &[Case<'_>] = &[
            (r"/s/a\040b/sub", b"/s/a b", Some(b"/s/a b/sub")),
            (r"/s/t\011ab/sub", b"/s/t\tab", Some(b"/s/t\tab/sub")),
            (r"/s/n\012l/sub", b"/s/n\nl", Some(b"/s/n\nl/sub")),
            (r"/s/b\134s/sub", b"/s/b\\s", Some(b"/s/b\\s/sub")),
            // A directory literally named `x\040y`: mountinfo escapes the
            // backslash.
            (r"/s/x\134040y/sub", b"/s/x\\040y", Some(b"/s/x\\040y/sub")),
            (r"/s/x\134040y/sub", b"/s/x y", None),
            // A mount point removed while mounted.
            (
                r"/s/r/sub\040(deleted)",
                b"/s/r",
                Some(b"/s/r/sub (deleted)"),
            ),
            // Siblings that share the root's prefix.
            (r"/s/root\040x", b"/s/root", None),
            (r"/s/root\012x", b"/s/root", None),
            (r"/s/rootx/sub", b"/s/root", None),
            // The root itself.
            (r"/s/root", b"/s/root", None),
            (r"/proc", b"/", Some(b"/proc")),
        ];
        for (mp, root, want) in cases {
            let info = format!("22 1 8:1 / / rw - ext4 /dev/sda1 rw\n{}", line(mp));
            assert_eq!(
                submount_below(info.as_bytes(), root).as_deref(),
                *want,
                "{mp} below {}",
                String::from_utf8_lossy(root)
            );
        }
        // Optional fields (`shared:N`, `master:N`) do not shift the mount
        // point.
        let info = b"40 22 0:9 / /s/root/sub rw,nodev shared:5 master:1 - tmpfs t rw\n";
        assert_eq!(
            submount_below(info, b"/s/root").as_deref(),
            Some(&b"/s/root/sub"[..])
        );
    }

    /// Red team (T2, the VM only; ADR-0060, T18): no mount of a full
    /// sandbox's root tree lets a device node open, wherever the node comes
    /// from: a mount two levels below the root, a read-only bind of a host
    /// directory below the root, a `RUN --mount` bind of a directory
    /// holding one, or a node a `RUN` makes with `mknod` on a submount or
    /// in the bind and a later `RUN` opens. The sandbox's own `/dev/null`
    /// opens. Same isolation as `a_mount_below_the_root_is_nodev_in_the_run`:
    /// a thread in a private mount namespace, every mount detached before
    /// its directory is removed.
    #[test]
    #[ignore = "T2: mounts and mknod as real root in a private mount namespace; run in the test VM only"]
    fn red_team_no_mount_of_the_root_tree_lets_a_device_node_open() {
        std::thread::spawn(|| {
            use rustix::mount::MountPropagationFlags;
            use rustix::thread::UnshareFlags;
            // SAFETY: this thread shares nothing that the unshared
            // filesystem attributes or mount namespace would break; it ends
            // with the test.
            unsafe { rustix::thread::unshare_unsafe(UnshareFlags::FS | UnshareFlags::NEWNS) }
                .expect("unshare(CLONE_FS | CLONE_NEWNS)");
            rustix::mount::mount_change(
                "/",
                MountPropagationFlags::PRIVATE | MountPropagationFlags::REC,
            )
            .expect("make every mount private");
            assert!(mount_setattr_supported(), "the VM kernel has mount_setattr");
            let mknod = |p: &Path| {
                rustix::fs::mknodat(
                    rustix::fs::CWD,
                    p,
                    FileType::CharacterDevice,
                    Mode::from_raw_mode(0o666),
                    rustix::fs::makedev(1, 3),
                )
                .unwrap_or_else(|e| panic!("mknod {}: {e}", p.display()));
            };
            let mut mounts = TestMounts::new(tempfile::tempdir().unwrap());
            let top = mounts.path().canonicalize().unwrap();
            let root = top.join("root");
            let outside = top.join("outside");
            let cache = top.join("cache");
            for d in ["usr", "sub", "robind", "cache"] {
                fs::create_dir_all(root.join(d)).unwrap();
            }
            fs::create_dir(&outside).unwrap();
            fs::create_dir(&cache).unwrap();
            for l in ["bin", "lib", "lib64", "sbin"] {
                std::os::unix::fs::symlink(format!("usr/{l}"), root.join(l)).unwrap();
            }
            mknod(&outside.join("null"));
            mknod(&cache.join("null"));
            rustix::mount::mount_bind("/usr", root.join("usr")).expect("bind /usr");
            mounts.add(root.join("usr"));
            rustix::mount::mount_remount(
                root.join("usr"),
                MountFlags::BIND | MountFlags::RDONLY,
                c"",
            )
            .expect("remount the /usr bind read-only");
            let tmpfs = |at: &Path| {
                rustix::mount::mount("tmpfs", at, "tmpfs", MountFlags::empty(), None)
                    .unwrap_or_else(|e| panic!("tmpfs at {}: {e}", at.display()));
            };
            tmpfs(&root.join("sub"));
            mounts.add(root.join("sub"));
            fs::create_dir(root.join("sub/deeper")).unwrap();
            tmpfs(&root.join("sub/deeper"));
            mounts.add(root.join("sub/deeper"));
            mknod(&root.join("sub/deeper/null"));
            // Read-only: neither the `RUN` nor a removal reaches `outside`
            // through it.
            rustix::mount::mount_bind(&outside, root.join("robind")).expect("bind outside");
            mounts.add(root.join("robind"));
            rustix::mount::mount_remount(
                root.join("robind"),
                MountFlags::BIND | MountFlags::RDONLY,
                c"",
            )
            .expect("remount the bind read-only");
            let runner = Runner::new(&top.join("sandbox"), Some(RootEntry::PivotRoot)).unwrap();
            let parent = ParentPid::new().unwrap();
            let sh = |script: &str| {
                let mut spec =
                    RunSpec::new(&root, vec!["/bin/sh".into(), "-c".into(), script.into()]);
                spec.mounts.push(crate::BindMount {
                    source: cache.clone(),
                    target: "/cache".into(),
                    kind: MountKind::Directory,
                    read_only: false,
                });
                runner
                    .run(spec, None, |job| clone_child(job, parent))
                    .unwrap()
            };
            let control = sh("echo x > /dev/null");
            let opened: Vec<(&str, ExitStatus)> = [
                ("deeper", "echo x > /sub/deeper/null"),
                ("robind", "echo x > /robind/null"),
                ("cache", "echo x > /cache/null"),
            ]
            .into_iter()
            .map(|(n, s)| (n, sh(s)))
            .collect();
            let made = sh("mknod /sub/deeper/made c 1 3 && mknod /cache/made c 1 3");
            let made_opens = [
                ("made on a submount", sh("echo x > /sub/deeper/made")),
                ("made in the RUN bind", sh("echo x > /cache/made")),
            ];
            drop(mounts);
            assert!(control.success(), "the sandbox's /dev/null: {control:?}");
            for (n, st) in &opened {
                assert!(!st.success(), "a device node opens via {n}: {st:?}");
            }
            assert!(made.success(), "the RUN may mknod (CAP_MKNOD): {made:?}");
            for (n, st) in &made_opens {
                assert!(!st.success(), "a node {n} opens in a later RUN: {st:?}");
            }
        })
        .join()
        .unwrap();
    }

    /// Runs `body` as a T2 test does: on a thread of its own, in a new
    /// mount namespace with every mount private, so that nothing it mounts
    /// reaches another namespace.
    pub(super) fn in_private_mount_namespace(body: impl FnOnce() + Send + 'static) {
        std::thread::spawn(|| {
            use rustix::mount::MountPropagationFlags;
            use rustix::thread::UnshareFlags;
            // SAFETY: this thread shares nothing that the unshared
            // filesystem attributes or mount namespace would break; it ends
            // with the test.
            unsafe { rustix::thread::unshare_unsafe(UnshareFlags::FS | UnshareFlags::NEWNS) }
                .expect("unshare(CLONE_FS | CLONE_NEWNS)");
            rustix::mount::mount_change(
                "/",
                MountPropagationFlags::PRIVATE | MountPropagationFlags::REC,
            )
            .expect("make every mount private");
            body();
        })
        .join()
        .unwrap();
    }

    /// A root at `root` that runs `/bin/sh`: the host's `/usr` bound
    /// read-only, so that neither a `RUN` nor a removal reaches it, and the
    /// `bin`, `lib`, `lib64`, `sbin` symlinks into it.
    pub(super) fn root_with_usr(mounts: &mut TestMounts, root: &Path) {
        fs::create_dir_all(root.join("usr")).unwrap();
        for l in ["bin", "lib", "lib64", "sbin"] {
            std::os::unix::fs::symlink(format!("usr/{l}"), root.join(l)).unwrap();
        }
        rustix::mount::mount_bind("/usr", root.join("usr")).expect("bind /usr");
        mounts.add(root.join("usr"));
        rustix::mount::mount_remount(root.join("usr"), MountFlags::BIND | MountFlags::RDONLY, c"")
            .expect("remount the /usr bind read-only");
    }

    /// A file outside the root for a `RUN`'s stdout and stderr, so that
    /// what it prints survives a root it made read-only.
    /// Its stdin is the host's `/dev/null`, so that a `RUN` whose own
    /// `/dev/null` no longer opens still starts.
    pub(super) fn output_to(path: &Path, spec: &mut RunSpec) {
        let file = File::create(path).unwrap();
        spec.stdin = Stdio::Fd(File::open("/dev/null").unwrap().into());
        spec.stdout = Stdio::Fd(file.try_clone().unwrap().into());
        spec.stderr = Stdio::Fd(file.into());
    }

    /// `Seccomp_filters` of `/proc/self/status` (Linux 5.9): how many
    /// filters the process runs under.
    pub(super) fn seccomp_filters(status: &str) -> Option<u32> {
        status
            .lines()
            .find_map(|l| l.strip_prefix("Seccomp_filters:"))
            .and_then(|n| n.trim().parse().ok())
    }

    /// T2 (ADR-0103, ADR-0109): a full-sandbox `RUN` runs under Docker's
    /// default profile, one filter more than ranikust, which refuses
    /// `personality(ADDR_NO_RANDOMIZE)` (`setarch -R`);
    /// `--seccomp=unconfined` adds none; a profile file's own rule takes
    /// effect in its place. Each `RUN` runs in a session keyring of its own
    /// (T11), joined before the profile: a profile that refuses the join
    /// with an errno the child does not take for "unsupported" still lets
    /// the `RUN` start. Its init runs under the same filters as its
    /// command, each installing its own: ADR-0033's filter under a held
    /// network adds one to both, unconfined too.
    #[test]
    #[ignore = "T2: mounts as real root in a private mount namespace; run in the test VM only"]
    fn a_run_gets_its_seccomp_profile() {
        in_private_mount_namespace(|| {
            let mut mounts = TestMounts::new(tempfile::tempdir().unwrap());
            let top = mounts.path().canonicalize().unwrap();
            let root = top.join("root");
            root_with_usr(&mut mounts, &root);
            let parent = ParentPid::new().unwrap();
            let mut runner = Runner::new(&top.join("sandbox"), Some(RootEntry::PivotRoot)).unwrap();
            runner.hold_networks();
            let own = seccomp_filters(&fs::read_to_string("/proc/self/status").unwrap())
                .expect("Seccomp_filters in /proc/self/status");
            let file = SeccompProfile::from_json(
                br#"{"defaultAction": "SCMP_ACT_ALLOW", "syscalls": [
                    {"names": ["uname"], "action": "SCMP_ACT_ERRNO", "errnoRet": 13}]}"#,
            )
            .unwrap();
            // `KEYCTL_JOIN_SESSION_KEYRING` is 1; errno 99 is not one the
            // child takes for "unsupported".
            let join = SeccompProfile::from_json(
                br#"{"defaultAction": "SCMP_ACT_ALLOW", "syscalls": [
                    {"names": ["keyctl"], "action": "SCMP_ACT_ERRNO", "errnoRet": 99,
                     "args": [{"index": 0, "value": 1, "op": "SCMP_CMP_EQ"}]}]}"#,
            )
            .unwrap();
            // SAFETY: a plain syscall that reads no memory.
            let ours = unsafe {
                libc::syscall(
                    libc::SYS_keyctl,
                    libc::c_long::from(libc::KEYCTL_GET_KEYRING_ID),
                    libc::c_long::from(libc::KEY_SPEC_SESSION_KEYRING),
                    libc::c_long::from(0),
                )
            };
            assert!(
                ours > 0,
                "the test's session keyring: {}",
                std::io::Error::last_os_error()
            );
            // `keyring=-1` where the profile refuses `keyctl`.
            let probe = format!(
                "cat /proc/self/status; \
                 grep -E '^Seccomp(_filters)?:' /proc/1/status | sed 's/^/init-/'; \
                 if setarch -R true; then echo personality=ok; else echo personality=refused; fi; \
                 if uname >/dev/null; then echo uname=ok; else echo uname=refused; fi; \
                 python3 -c 'import ctypes as c; print(\"keyring=%d\" % c.CDLL(None).syscall(\
                 c.c_long({}), c.c_long({}), c.c_long({}), c.c_long(0)))'",
                libc::SYS_keyctl,
                libc::KEYCTL_GET_KEYRING_ID,
                libc::KEY_SPEC_SESSION_KEYRING,
            );
            let mut got = Vec::new();
            for (name, profile, network) in [
                (
                    "default",
                    Some(SeccompProfile::docker_default().unwrap()),
                    Network::Host,
                ),
                ("unconfined", None, Network::Host),
                ("file", Some(file), Network::Host),
                ("join", Some(join), Network::Host),
                ("unconfined, held network", None, Network::None),
            ] {
                runner.seccomp = profile;
                let out = top.join(format!("out-{name}"));
                let mut spec = RunSpec::new(
                    &root,
                    vec!["/bin/sh".into(), "-c".into(), probe.clone().into()],
                );
                spec.network = network;
                output_to(&out, &mut spec);
                let st = runner.run(spec, None, |job| clone_child(job, parent));
                let text = fs::read_to_string(&out).unwrap_or_default();
                println!("{name}: {st:?}\n{text}");
                got.push((name, st.map(|s| s.success()).ok(), text));
            }
            drop(mounts);
            let mut keyrings = vec![ours];
            for ((name, ok, text), (filters, personality, uname, keyctl)) in got.iter().zip([
                (own + 1, "personality=refused", "uname=ok", false),
                (own, "personality=ok", "uname=ok", true),
                (own + 1, "personality=ok", "uname=refused", true),
                (own + 1, "personality=ok", "uname=ok", true),
                (own + 1, "personality=ok", "uname=ok", true),
            ]) {
                assert_eq!(*ok, Some(true), "{name}: the RUN ran: {text}");
                assert_eq!(seccomp_filters(text), Some(filters), "{name}: {text}");
                let mode = if filters == 0 { "0" } else { "2" };
                for key in ["Seccomp", "init-Seccomp"] {
                    assert_eq!(status_value(text, key), Some(mode), "{name}: {key}: {text}");
                }
                let count = filters.to_string();
                assert_eq!(
                    status_value(text, "init-Seccomp_filters"),
                    Some(count.as_str()),
                    "{name}: the init's filters: {text}"
                );
                assert!(text.contains(personality), "{name}: {personality}: {text}");
                assert!(text.contains(uname), "{name}: {uname}: {text}");
                let keyring: Option<i64> = text
                    .lines()
                    .find_map(|l| l.strip_prefix("keyring="))
                    .and_then(|n| n.trim().parse().ok());
                if keyctl {
                    let k = keyring
                        .filter(|&k| k > 0)
                        .unwrap_or_else(|| panic!("{name}: no keyring id: {text}"));
                    assert!(
                        !keyrings.contains(&k),
                        "{name}: keyring {k} not new: {text}"
                    );
                    keyrings.push(k);
                } else {
                    assert_eq!(keyring, Some(-1), "{name}: keyctl refused: {text}");
                }
            }
        });
    }

    /// T2: a profile whose default action kills (`SCMP_ACT_KILL_PROCESS`)
    /// or traps (`SCMP_ACT_TRAP`) kills the `RUN`'s set-up at its first call
    /// after the filters. The `RUN` fails as `Error::SetupKilled`, its init
    /// killed by `SIGSYS` under its profile, never with an exit status of
    /// the command, which never ran.
    #[test]
    #[ignore = "T2: mounts as real root in a private mount namespace; run in the test VM only"]
    fn a_set_up_its_profile_kills_is_reported_as_killed() {
        in_private_mount_namespace(|| {
            let mut mounts = TestMounts::new(tempfile::tempdir().unwrap());
            let top = mounts.path().canonicalize().unwrap();
            let root = top.join("root");
            root_with_usr(&mut mounts, &root);
            let parent = ParentPid::new().unwrap();
            let mut runner = Runner::new(&top.join("sandbox"), Some(RootEntry::PivotRoot)).unwrap();
            let mut got = Vec::new();
            for action in ["SCMP_ACT_KILL_PROCESS", "SCMP_ACT_TRAP"] {
                let json = format!(r#"{{"defaultAction": "{action}"}}"#);
                runner.seccomp = Some(SeccompProfile::from_json(json.as_bytes()).unwrap());
                let spec = RunSpec::new(&root, vec!["/bin/true".into()]);
                got.push((
                    action,
                    runner.run(spec, None, |job| clone_child(job, parent)),
                ));
            }
            drop(mounts);
            for (action, r) in got {
                assert!(
                    matches!(
                        r,
                        Err(Error::SetupKilled {
                            signal: libc::SIGSYS,
                            seccomp: true
                        })
                    ),
                    "{action}: {r:?}"
                );
            }
        });
    }

    /// A profile of rules on a default that allows; errno 0 compiles, as a
    /// filter around ranikust may answer with it, though `--seccomp` refuses
    /// it (ADR-0109).
    fn allow_but(rules: &str) -> SeccompProfile {
        SeccompProfile::faking(
            format!(r#"{{"defaultAction": "SCMP_ACT_ALLOW", "syscalls": [{rules}]}}"#).as_bytes(),
        )
        .unwrap()
    }

    /// Installs [`allow_but`]`(rules)` on this thread only, standing in for
    /// a filter around ranikust; as root, so without `no_new_privs`. It
    /// stays until the thread ends; each stacks on the last, and the
    /// newest's errno wins.
    fn filter_this_thread(rules: &str) {
        let prog = allow_but(rules).program;
        let fprog = libc::sock_fprog {
            len: u16::try_from(prog.insns.len()).unwrap(),
            filter: prog.insns.as_ptr().cast_mut(),
        };
        // SAFETY: `seccomp(SECCOMP_SET_MODE_FILTER)` reads the program,
        // which lives across the call.
        let r = unsafe {
            libc::syscall(
                libc::SYS_seccomp,
                libc::SECCOMP_SET_MODE_FILTER,
                0u32,
                &raw const fprog,
            )
        };
        assert_eq!(r, 0, "{}", std::io::Error::last_os_error());
    }

    /// A rule answering `name` (with `arg0`, if given) with `errno`, 0
    /// being a faked success.
    fn errno_rule(name: &str, errno: i32, arg0: Option<u32>) -> String {
        let args = arg0.map_or(String::new(), |v| {
            format!(r#","args": [{{"index": 0, "value": {v}, "op": "SCMP_CMP_EQ"}}]"#)
        });
        format!(r#"{{"names": ["{name}"], "action": "SCMP_ACT_ERRNO", "errnoRet": {errno}{args}}}"#)
    }

    /// The value of `key:` in a `/proc/<pid>/status` dump.
    fn status_value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
        text.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))
            .map(str::trim)
    }

    /// T2 (ADR-0109): every step that keeps a `RUN` in comes before its
    /// profile, which may answer any call with an errno the child would
    /// take for "unsupported" (`EINVAL`); the cases with a faked success
    /// (errno 0), which `--seccomp` refuses, stand for a profile that could
    /// answer so. Under such a profile the `RUN`'s bounding set is still Docker's defaults,
    /// its inheritable and ambient sets stay empty (with ranikust holding
    /// `CAP_SYS_ADMIN` in both, which a root `execve` would add back; the
    /// ambient set goes with the inheritable one, so the case cannot tell
    /// the two clears apart), its session belongs to its PID namespace, so
    /// no terminal of the operator's is its controlling one, its init is
    /// not dumpable (its `/proc/1/environ` stays closed to the command),
    /// its umask is 022 whatever ranikust's is, and ADR-0033's filter is
    /// still installed under a held network namespace: `unshare -U` is
    /// refused, which a control without that filter shows it would not be.
    /// The init's fork comes before the profile too (ADR-0110), so a
    /// profile faking it and the command's `setsid` still runs the `RUN`,
    /// under its profile. The same calls faked by a filter around ranikust
    /// (one on a test thread stands in) fail the `RUN` before the command
    /// runs, never leaving it as PID 1 without an init, and so does a
    /// filter install refused there, as the filter's step.
    #[test]
    #[ignore = "T2: mounts as real root in a private mount namespace; run in the test VM only"]
    fn a_profile_cannot_fake_the_steps_before_it() {
        // `PR_SET_DUMPABLE`, `PR_CAPBSET_DROP` and `PR_CAP_AMBIENT`
        // (`linux/prctl.h`).
        const SET_DUMPABLE: u32 = 4;
        const CAPBSET_DROP: u32 = 24;
        const CAP_AMBIENT: u32 = 47;
        in_private_mount_namespace(|| {
            let mut mounts = TestMounts::new(tempfile::tempdir().unwrap());
            let top = mounts.path().canonicalize().unwrap();
            let root = top.join("root");
            root_with_usr(&mut mounts, &root);
            let parent = ParentPid::new().unwrap();
            let mut runner = Runner::new(&top.join("sandbox"), Some(RootEntry::PivotRoot)).unwrap();
            runner.hold_networks();
            let own = seccomp_filters(&fs::read_to_string("/proc/self/status").unwrap())
                .expect("Seccomp_filters in /proc/self/status");
            // The umask from `/proc`: the shell's `umask` would call umask(2).
            let probe = "grep -E '^(Cap|Seccomp_filters|Umask)' /proc/self/status; \
                 python3 -c 'import os; print(\"sid=%d\" % os.getsid(0))'; \
                 if unshare -U true; then echo userns=ok; else echo userns=refused; fi; \
                 if cat /proc/1/environ >/dev/null 2>&1; then echo environ=read; \
                 else echo environ=refused; fi";
            let defaults = format!("{:016x}", caps::docker_default_mask());
            // `clone(SIGCHLD, ...)`, the init's fork of the command, and
            // `setsid`: after a faked fork the init goes on as the command,
            // and its second `setsid`, as a session leader, would fail and
            // hide a missing parent check.
            let fork = format!(
                "{},{}",
                errno_rule("clone", 0, Some(libc::SIGCHLD.unsigned_abs())),
                errno_rule("setsid", 0, None)
            );
            let cases = [
                (
                    "bounding, faked",
                    errno_rule("prctl", 0, Some(CAPBSET_DROP)),
                    Network::Host,
                ),
                (
                    "bounding, EINVAL",
                    errno_rule("prctl", libc::EINVAL, Some(CAPBSET_DROP)),
                    Network::Host,
                ),
                (
                    "setsid, faked",
                    errno_rule("setsid", 0, None),
                    Network::Host,
                ),
                (
                    "dumpable, faked",
                    errno_rule("prctl", 0, Some(SET_DUMPABLE)),
                    Network::Host,
                ),
                ("umask, faked", errno_rule("umask", 0, None), Network::Host),
                ("fork, faked", fork.clone(), Network::Host),
                (
                    "seccomp, faked, held network",
                    errno_rule("seccomp", 0, None),
                    Network::None,
                ),
                (
                    "seccomp, faked, control",
                    errno_rule("seccomp", 0, None),
                    Network::Host,
                ),
                (
                    "capset and ambient, faked",
                    format!(
                        "{},{}",
                        errno_rule("capset", 0, None),
                        errno_rule("prctl", 0, Some(CAP_AMBIENT))
                    ),
                    Network::Host,
                ),
            ];
            let mut got = Vec::new();
            for (name, rules, network) in cases {
                if name.starts_with("capset") {
                    // Last of the profile cases: this thread keeps them.
                    // Root's `execve` adds the inheritable set to the
                    // permitted one.
                    let sets = rustix::thread::capabilities(None).unwrap();
                    rustix::thread::set_capabilities(
                        None,
                        rustix::thread::CapabilitySets {
                            inheritable: rustix::thread::CapabilitySet::SYS_ADMIN,
                            ..sets
                        },
                    )
                    .unwrap();
                    rustix::thread::configure_capability_in_ambient_set(
                        rustix::thread::CapabilitySet::SYS_ADMIN,
                        true,
                    )
                    .unwrap();
                }
                runner.seccomp = Some(allow_but(&rules));
                let out = top.join(format!("out-{}", got.len()));
                let mut spec =
                    RunSpec::new(&root, vec!["/bin/sh".into(), "-c".into(), probe.into()]);
                spec.network = network;
                output_to(&out, &mut spec);
                // This thread's own (it unshared `CLONE_FS`), and the `RUN` is cloned from it.
                let ours = name
                    .starts_with("umask")
                    .then(|| rustix::process::umask(Mode::from_raw_mode(0o077)));
                let st = runner.run(spec, None, |job| clone_child(job, parent));
                if let Some(mask) = ours {
                    rustix::process::umask(mask);
                }
                let text = fs::read_to_string(&out).unwrap_or_default();
                println!("{name}: {st:?}\n{text}");
                got.push((name, st.map(|s| s.success()).ok(), text));
            }
            // An install refused around ranikust, on a thread of its own so
            // that this one can still install the next filter: the init and
            // the command each fail at it, before the command runs.
            let (refused, refused_text) = std::thread::scope(|s| {
                s.spawn(|| {
                    filter_this_thread(&errno_rule("seccomp", libc::EPERM, None));
                    runner.seccomp = Some(SeccompProfile::docker_default().unwrap());
                    let out = top.join("out-install-refused");
                    let mut spec =
                        RunSpec::new(&root, vec!["/bin/sh".into(), "-c".into(), probe.into()]);
                    output_to(&out, &mut spec);
                    let st = runner.run(spec, None, |job| clone_child(job, parent));
                    (st, fs::read_to_string(&out).unwrap())
                })
                .join()
                .unwrap()
            });
            println!("install, refused around ranikust: {refused:?}\n{refused_text}");
            // Last, as this thread keeps it: the fork faked around ranikust.
            // The `RUN` fails, and the command never runs, so it writes
            // nothing.
            filter_this_thread(&fork);
            runner.seccomp = None;
            let out = top.join("out-fork-around");
            let mut spec = RunSpec::new(&root, vec!["/bin/sh".into(), "-c".into(), probe.into()]);
            output_to(&out, &mut spec);
            let forked = runner.run(spec, None, |job| clone_child(job, parent));
            let forked_text = fs::read_to_string(&out).unwrap();
            println!("fork, faked around ranikust: {forked:?}\n{forked_text}");
            drop(mounts);
            let mut wrong = Vec::new();
            let parent_checked = matches!(
                &forked,
                Err(Error::Setup { step: Step::Init, source })
                    if source.raw_os_error() == Some(libc::EPROTO)
            );
            if !parent_checked || !forked_text.is_empty() {
                wrong.push(format!(
                    "fork, faked around ranikust: {forked:?}, output {forked_text:?}"
                ));
            }
            let install_failed = matches!(
                &refused,
                Err(Error::Setup { step: Step::Seccomp, source })
                    if source.raw_os_error() == Some(libc::EPERM)
            );
            if !install_failed || !refused_text.is_empty() {
                wrong.push(format!(
                    "install, refused around ranikust: {refused:?}, output {refused_text:?}"
                ));
            }
            for (name, ok, text) in &got {
                let mut want: Vec<(&str, String)> = vec![
                    ("CapBnd", defaults.clone()),
                    ("environ", "refused".into()),
                    ("Umask", "0022".into()),
                ];
                match *name {
                    "seccomp, faked, held network" => {
                        want.push(("Seccomp_filters", (own + 2).to_string()));
                        want.push(("userns", "refused".into()));
                    }
                    "seccomp, faked, control" => want.push(("userns", "ok".into())),
                    // The command still installed its profile.
                    "fork, faked" => want.push(("Seccomp_filters", (own + 1).to_string())),
                    "capset and ambient, faked" => {
                        for key in ["CapInh", "CapAmb"] {
                            want.push((key, format!("{:016x}", 0)));
                        }
                        for key in ["CapPrm", "CapEff"] {
                            want.push((key, defaults.clone()));
                        }
                    }
                    _ => {}
                }
                if *ok != Some(true) {
                    wrong.push(format!("{name}: the RUN did not run"));
                }
                for (key, value) in want {
                    let seen = match key {
                        "userns" | "environ" => text
                            .lines()
                            .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
                            .map(str::trim),
                        _ => status_value(text, key),
                    };
                    if seen != Some(value.as_str()) {
                        wrong.push(format!("{name}: {key} is {seen:?}, not {value}"));
                    }
                }
                // 0 when the session's leader is outside the `RUN`'s PID
                // namespace: ranikust's session, and its terminal.
                let sid = text.lines().find_map(|l| l.strip_prefix("sid="));
                if !sid.is_some_and(|s| s.trim().parse::<u32>().is_ok_and(|s| s > 0)) {
                    wrong.push(format!("{name}: session id {sid:?}"));
                }
            }
            assert!(wrong.is_empty(), "{}", wrong.join("\n"));
        });
    }

    /// T2 (ADR-0080): a `RUN`'s command starts only once its init has
    /// finished every step. Under a profile that fails one of the init's
    /// last steps, or its closes (`close` and `close_range`, or
    /// `close_range` and the `close` of one descriptor, whatever the
    /// errno), the `RUN` fails as `Step::Init` with that errno, before its
    /// command ran, and the command leaves no mark, which it leaves under a
    /// control profile that lets the init finish. These check the outcome;
    /// the go byte itself is checked in `child.rs`. A profile that refuses
    /// `close` alone holds nothing up: the command starts on the byte, not
    /// on the end of the pipe, and the `RUN` ends with its status.
    #[test]
    #[ignore = "T2: mounts as real root in a private mount namespace; run in the test VM only"]
    fn a_runs_command_starts_only_once_its_init_has_finished() {
        in_private_mount_namespace(|| {
            let mut mounts = TestMounts::new(tempfile::tempdir().unwrap());
            let top = mounts.path().canonicalize().unwrap();
            let root = top.join("root");
            root_with_usr(&mut mounts, &root);
            let parent = ParentPid::new().unwrap();
            let mut runner = Runner::new(&top.join("sandbox"), Some(RootEntry::PivotRoot)).unwrap();
            let pdeathsig = libc::PR_GET_PDEATHSIG.unsigned_abs();
            // Calls only the init makes after its filters.
            let init_steps = format!(
                "{},{}",
                errno_rule("prctl", libc::EPERM, Some(pdeathsig)),
                errno_rule("kill", libc::EPERM, None)
            );
            // What a case gives: the command's mark and success, the
            // command's own exit status, or a failure as `Step::Init` with
            // that errno, the command unrun.
            enum Want {
                Runs,
                Ends,
                Fails(i32),
            }
            let with_range =
                |close: String| format!("{close},{}", errno_rule("close_range", libc::EPERM, None));
            let cases = [
                ("control", errno_rule("kill", libc::EPERM, None), Want::Runs),
                ("the init fails", init_steps, Want::Fails(libc::EPERM)),
                (
                    "close refused",
                    errno_rule("close", libc::EPERM, None),
                    Want::Ends,
                ),
                (
                    "closes refused",
                    with_range(errno_rule("close", libc::EPERM, None)),
                    Want::Fails(libc::EPERM),
                ),
                (
                    "one close refused",
                    with_range(errno_rule("close", libc::EPERM, Some(0))),
                    Want::Fails(libc::EPERM),
                ),
                (
                    "one close refused with EBADF",
                    with_range(errno_rule("close", libc::EBADF, Some(0))),
                    Want::Fails(libc::EBADF),
                ),
            ];
            let mut got = Vec::new();
            for (n, (name, rules, want)) in cases.into_iter().enumerate() {
                runner.seccomp = Some(allow_but(&rules));
                let mark = format!("mark-{n}");
                let out = top.join(format!("out-{n}"));
                let script = format!("echo ran > /{mark}");
                let mut spec =
                    RunSpec::new(&root, vec!["/bin/sh".into(), "-c".into(), script.into()]);
                output_to(&out, &mut spec);
                // A `RUN` that waits for a go it never gets is cancelled,
                // which fails the case rather than the whole suite.
                let running = &Running::default();
                let (done, wait) = std::sync::mpsc::channel::<()>();
                let st = std::thread::scope(|s| {
                    s.spawn(move || {
                        let waited = wait.recv_timeout(std::time::Duration::from_secs(60));
                        if waited == Err(std::sync::mpsc::RecvTimeoutError::Timeout) {
                            running.cancel();
                        }
                    });
                    let st = runner.run(spec, Some(running), |job| clone_child(job, parent));
                    drop(done);
                    st
                });
                let text = fs::read_to_string(&out).unwrap_or_default();
                let marked = fs::read_to_string(root.join(&mark)).ok();
                println!("{name}: {st:?}, mark {marked:?}\n{text}");
                got.push((name, want, st, marked, text));
            }
            drop(mounts);
            let mut wrong = Vec::new();
            for (name, want, st, marked, text) in &got {
                let ok = match *want {
                    Want::Fails(errno) => {
                        marked.is_none()
                            && text.is_empty()
                            && matches!(
                                st,
                                Err(e @ Error::Setup { step: Step::Init, source })
                                    if source.raw_os_error() == Some(errno)
                                        && e.ran_no_command()
                            )
                    }
                    // The shell's loader needs `close` too: the command's own
                    // exit status, rather than a cancel, shows it started.
                    Want::Ends => st.is_ok(),
                    Want::Runs => {
                        marked.as_deref() == Some("ran\n")
                            && st.as_ref().is_ok_and(ExitStatus::success)
                    }
                };
                if !ok {
                    wrong.push(format!("{name}: {st:?}, mark {marked:?}, output {text:?}"));
                }
            }
            assert!(wrong.is_empty(), "{}", wrong.join("\n"));
        });
    }

    /// T2 (ADR-0080): an init that ends without the command's status never
    /// gives its own as the command's. Under a profile refusing `wait4`, the
    /// init's wait fails and it exits 125, which fails the `RUN` as
    /// `Error::InitExited`; under one refusing its status write, it exits 0,
    /// which fails the `RUN` as a status that could not be read, unless the
    /// command left a failure record, which wins. A control profile lets
    /// the `RUN` end with its command's status.
    #[test]
    #[ignore = "T2: mounts as real root in a private mount namespace; run in the test VM only"]
    fn an_init_that_ends_without_the_commands_status_fails_the_run_as_such() {
        in_private_mount_namespace(|| {
            let mut mounts = TestMounts::new(tempfile::tempdir().unwrap());
            let top = mounts.path().canonicalize().unwrap();
            let root = top.join("root");
            root_with_usr(&mut mounts, &root);
            let parent = ParentPid::new().unwrap();
            let mut runner = Runner::new(&top.join("sandbox"), Some(RootEntry::PivotRoot)).unwrap();
            // The init's status write is its only write of four bytes after
            // its filters; `/bin/true` and a command that fails to start
            // make none.
            let status_write = format!(
                r#"{{"names": ["write"], "action": "SCMP_ACT_ERRNO", "errnoRet": {},
                    "args": [{{"index": 2, "value": {}, "op": "SCMP_CMP_EQ"}}]}}"#,
                libc::EPERM,
                child::STATUS_LEN
            );
            let cases = [
                ("control", String::new(), "/bin/true"),
                (
                    "wait refused",
                    errno_rule("wait4", libc::EPERM, None),
                    "/bin/true",
                ),
                ("status write refused", status_write.clone(), "/bin/true"),
                ("status write refused, no command", status_write, "/missing"),
            ];
            let mut got = Vec::new();
            for (name, rules, program) in cases {
                runner.seccomp = Some(allow_but(&rules));
                let spec = RunSpec::new(&root, vec![program.into()]);
                let r = runner.run(spec, None, |job| clone_child(job, parent));
                println!("{name}: {r:?}");
                got.push((name, r));
            }
            drop(mounts);
            let mut wrong = Vec::new();
            for (name, r) in &got {
                let ok = match *name {
                    "control" => r.as_ref().is_ok_and(ExitStatus::success),
                    "wait refused" => matches!(
                        r,
                        Err(e @ Error::InitExited { code: 125 }) if !e.ran_no_command()
                    ),
                    "status write refused" => matches!(
                        r,
                        Err(e @ Error::Sys { op: STATUS_OP, source })
                            if source.kind() == io::ErrorKind::InvalidData
                                && !e.ran_no_command()
                    ),
                    "status write refused, no command" => matches!(
                        r,
                        Err(e @ Error::Exec { program, source })
                            if program == "/missing"
                                && source.raw_os_error() == Some(libc::ENOENT)
                                && e.ran_no_command()
                    ),
                    other => unreachable!("no expectation for case {other:?}"),
                };
                if !ok {
                    wrong.push(format!("{name}: {r:?}"));
                }
            }
            assert!(wrong.is_empty(), "{}", wrong.join("\n"));
        });
    }

    /// T2 (ADR-0109): a refused join of the session keyring, by an LSM or
    /// a filter around ranikust (filters on this thread stand in for both),
    /// fails a `RUN` whose profile lets `keyctl`, `add_key` or `request_key` run
    /// (unconfined, or a file that allows `keyctl`) while ranikust's session
    /// keyring can still be looked up, whatever the errno. Under a profile
    /// that refuses all three, Docker's default or a file refusing them, the
    /// `RUN` runs. Under a filter that refuses `keyctl` whole, as Docker's
    /// default around ranikust does, the lookup is refused too and every
    /// `RUN` runs.
    #[test]
    #[ignore = "T2: mounts as real root in a private mount namespace; run in the test VM only"]
    fn a_refused_keyring_join_fails_a_run_whose_profile_allows_keyctl() {
        in_private_mount_namespace(|| {
            let mut mounts = TestMounts::new(tempfile::tempdir().unwrap());
            let top = mounts.path().canonicalize().unwrap();
            let root = top.join("root");
            root_with_usr(&mut mounts, &root);
            let parent = ParentPid::new().unwrap();
            let mut runner = Runner::new(&top.join("sandbox"), Some(RootEntry::PivotRoot)).unwrap();
            let keyrings = ["keyctl", "add_key", "request_key"]
                .map(|n| errno_rule(n, libc::EPERM, None))
                .join(",");
            let default = || Some(SeccompProfile::docker_default().unwrap());
            // `KEYCTL_JOIN_SESSION_KEYRING` is 1.
            let join = |errno| errno_rule("keyctl", errno, Some(1));
            let phases = [
                (
                    join(libc::EPERM),
                    vec![
                        ("join EPERM, default", default(), None),
                        ("join EPERM, unconfined", None, Some(libc::EPERM)),
                    ],
                ),
                (
                    join(libc::EACCES),
                    vec![
                        ("join EACCES, default", default(), None),
                        (
                            "join EACCES, keyring calls refused",
                            Some(allow_but(&keyrings)),
                            None,
                        ),
                        ("join EACCES, unconfined", None, Some(libc::EACCES)),
                        (
                            "join EACCES, a file allowing keyctl",
                            Some(allow_but("")),
                            Some(libc::EACCES),
                        ),
                    ],
                ),
                (
                    errno_rule("keyctl", libc::EPERM, None),
                    vec![
                        ("keyctl EPERM, unconfined", None, None),
                        (
                            "keyctl EPERM, a file allowing keyctl",
                            Some(allow_but("")),
                            None,
                        ),
                    ],
                ),
            ];
            let mut got = Vec::new();
            for (outer, cases) in phases {
                filter_this_thread(&outer);
                for (name, profile, fails) in cases {
                    runner.seccomp = profile;
                    let spec = RunSpec::new(&root, vec!["/bin/true".into()]);
                    let r = runner.run(spec, None, |job| clone_child(job, parent));
                    println!("{name}: {r:?}");
                    got.push((name, fails, r));
                }
            }
            drop(mounts);
            for (name, fails, r) in got {
                match fails {
                    None => assert!(matches!(&r, Ok(st) if st.success()), "{name}: {r:?}"),
                    Some(errno) => assert!(
                        matches!(
                            &r,
                            Err(Error::Setup { step: Step::KeyringRefused, source })
                                if source.raw_os_error() == Some(errno)
                        ),
                        "{name}: {r:?}"
                    ),
                }
            }
        });
    }

    /// Red team (T2, the VM only; T1, T7): a descriptor of a directory
    /// outside the root, open in ranikust as the state directory and the
    /// build context are during a build, gives a `RUN` no way out: not a
    /// `WORKDIR` spelled `/proc/self/fd/<n>` or `/dev/fd/<n>`, not an image
    /// symlink to one, not a `PATH` entry through one, whichever way the
    /// child enters the root. A `RUN` that starts outside finds the dummy
    /// canary in its working directory.
    #[test]
    #[ignore = "T2: mounts as real root in a private mount namespace; run in the test VM only"]
    fn red_team_an_inherited_descriptor_gives_a_run_no_way_out() {
        in_private_mount_namespace(|| {
            let mut mounts = TestMounts::new(tempfile::tempdir().unwrap());
            let top = mounts.path().canonicalize().unwrap();
            let root = top.join("root");
            root_with_usr(&mut mounts, &root);
            let outside = top.join("outside");
            fs::create_dir(&outside).unwrap();
            fs::write(outside.join("rnk-canary"), b"dummy").unwrap();
            // A binary, not a script: an interpreter would reopen a script
            // by its path once `execve` closed the descriptor.
            fs::copy("/usr/bin/touch", outside.join("rnk-probe")).unwrap();
            let held = rustix::fs::open(
                &outside,
                OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .unwrap();
            let n = held.as_raw_fd();
            std::os::unix::fs::symlink(format!("/proc/self/fd/{n}"), root.join("w")).unwrap();
            let parent = ParentPid::new().unwrap();
            let mut escaped = Vec::new();
            for entry in [RootEntry::PivotRoot, RootEntry::MoveMount] {
                let runner =
                    Runner::new(&top.join(format!("sandbox-{entry:?}")), Some(entry)).unwrap();
                for workdir in [
                    format!("/proc/self/fd/{n}"),
                    format!("/dev/fd/{n}"),
                    "/w".to_owned(),
                ] {
                    let mut spec = RunSpec::new(
                        &root,
                        vec![
                            "/bin/sh".into(),
                            "-c".into(),
                            "if test -e rnk-canary; then echo outside; else echo inside; fi \
                             > /verdict"
                                .into(),
                        ],
                    );
                    spec.workdir = workdir.clone().into();
                    let r = runner.run(spec, None, |job| clone_child(job, parent));
                    let verdict = fs::read_to_string(root.join("verdict")).unwrap_or_default();
                    let _ = fs::remove_file(root.join("verdict"));
                    println!("{entry:?}, WORKDIR {workdir}: {r:?}, verdict {verdict:?}");
                    if verdict.trim() == "outside" {
                        escaped.push(format!("{entry:?}: WORKDIR {workdir}"));
                    }
                }
                let mut spec =
                    RunSpec::new(&root, vec!["rnk-probe".into(), "/verdict-exec".into()]);
                spec.env.push((
                    "PATH".into(),
                    format!("/proc/self/fd/{n}:/dev/fd/{n}:/w:/usr/bin").into(),
                ));
                let r = runner.run(spec, None, |job| clone_child(job, parent));
                let ran = root.join("verdict-exec").exists();
                let _ = fs::remove_file(root.join("verdict-exec"));
                println!("{entry:?}, PATH through descriptor {n}: {r:?}, ran {ran}");
                if ran {
                    escaped.push(format!("{entry:?}: PATH through descriptor {n}"));
                }
            }
            drop(held);
            drop(mounts);
            assert!(
                escaped.is_empty(),
                "a RUN started outside its root through an inherited descriptor: {escaped:?}"
            );
        });
    }

    /// Red team (T2, the VM only; T17): a root whose `/proc` is a symlink
    /// through a directory a later mount hides (`/sys` or `/dev`) does not
    /// get a `proc` without its read-only and masked paths: the `RUN` either
    /// fails or finds `/proc/sys` read-only, as in a root with an ordinary
    /// `/proc` (the control). Only `kernel.hostname` is written, which
    /// belongs to the `RUN`'s own UTS namespace.
    #[test]
    #[ignore = "T2: mounts as real root in a private mount namespace; run in the test VM only"]
    fn red_team_a_symlinked_proc_keeps_its_kernel_files_protected() {
        in_private_mount_namespace(|| {
            let mut mounts = TestMounts::new(tempfile::tempdir().unwrap());
            let top = mounts.path().canonicalize().unwrap();
            let runner = Runner::new(&top.join("sandbox"), Some(RootEntry::PivotRoot)).unwrap();
            let parent = ParentPid::new().unwrap();
            let mut unprotected = Vec::new();
            let mut control = None;
            // (case, the directory the symlink goes through, where proc is
            // in the RUN)
            for (i, (case, hop, proc_at)) in [
                ("through /sys", Some("sys"), "/realproc"),
                ("through /dev", Some("dev"), "/realproc"),
                ("control", None, "/proc"),
            ]
            .into_iter()
            .enumerate()
            {
                let root = top.join(format!("root{i}"));
                root_with_usr(&mut mounts, &root);
                if let Some(hop) = hop {
                    fs::create_dir(root.join("realproc")).unwrap();
                    fs::create_dir(root.join(hop)).unwrap();
                    std::os::unix::fs::symlink("/", root.join(hop).join("up")).unwrap();
                    std::os::unix::fs::symlink(format!("/{hop}/up/realproc"), root.join("proc"))
                        .unwrap();
                }
                let script = format!(
                    "grep -E ' {proc_at}(/sys)? ' {proc_at}/self/mountinfo; \
                     if [ -w {proc_at}/sys/kernel/hostname ]; then echo WRITABLE; fi; \
                     echo rnk-test > {proc_at}/sys/kernel/hostname && echo WROTE"
                );
                let mut spec =
                    RunSpec::new(&root, vec!["/bin/sh".into(), "-c".into(), script.into()]);
                let out = top.join(format!("out{i}"));
                output_to(&out, &mut spec);
                let r = runner.run(spec, None, |job| clone_child(job, parent));
                let text = fs::read_to_string(&out).unwrap();
                println!("{case}: {r:?}\n{text}");
                if hop.is_some() {
                    let left = fs::read_dir(root.join("realproc")).unwrap().count();
                    assert_eq!(left, 0, "{case}: the RUN left files in /realproc");
                }
                if text.contains("WROTE") || text.contains("WRITABLE") {
                    unprotected.push(case);
                }
                if hop.is_none() {
                    control = Some(r);
                }
            }
            drop(mounts);
            assert!(
                matches!(control, Some(Ok(_))),
                "the control RUN ran: {control:?}"
            );
            assert!(
                unprotected.is_empty(),
                "a writable /proc/sys reached the RUN: {unprotected:?}"
            );
        });
    }

    /// Red team (T2, the VM only; T7): a cache mounted over `/dev` whose
    /// `null` is a symlink to `/proc/self/fd/1` does not hand the `RUN`
    /// ranikust's own stdout as its stdin: a `RUN` with the default stdin
    /// reads the sandbox's `/dev/null` (character device 1:3), in both ways
    /// of binding.
    #[test]
    #[ignore = "T2: mounts as real root in a private mount namespace; run in the test VM only"]
    fn red_team_a_cache_over_dev_gets_no_stream_of_ranikusts() {
        in_private_mount_namespace(|| {
            let mut mounts = TestMounts::new(tempfile::tempdir().unwrap());
            let top = mounts.path().canonicalize().unwrap();
            let root = top.join("root");
            root_with_usr(&mut mounts, &root);
            let cache = top.join("cache");
            fs::create_dir(&cache).unwrap();
            std::os::unix::fs::symlink("/proc/self/fd/1", cache.join("null")).unwrap();
            let parent = ParentPid::new().unwrap();
            let mut wrong = Vec::new();
            for (n, mode) in ["mount API", "by path"].into_iter().enumerate() {
                let mut runner =
                    Runner::new(&top.join(format!("sandbox{n}")), Some(RootEntry::PivotRoot))
                        .unwrap();
                if mode == "by path" {
                    runner.without_mount_api();
                }
                let mut spec = RunSpec::new(
                    &root,
                    vec![
                        "/bin/sh".into(),
                        "-c".into(),
                        "stat -L -c '%F %t:%T' /proc/self/fd/0".into(),
                    ],
                );
                spec.mounts.push(crate::BindMount {
                    source: cache.clone(),
                    target: "/dev".into(),
                    kind: MountKind::Directory,
                    read_only: false,
                });
                // stdin stays the default, `Stdio::Null`.
                let out = top.join(format!("out{n}"));
                let file = File::create(&out).unwrap();
                spec.stdout = Stdio::Fd(file.try_clone().unwrap().into());
                spec.stderr = Stdio::Fd(file.into());
                let r = runner.run(spec, None, |job| clone_child(job, parent));
                let text = fs::read_to_string(&out).unwrap();
                println!("{mode}: {r:?}: {text}");
                if !matches!(&r, Ok(st) if st.success())
                    || text.trim() != "character special file 1:3"
                {
                    wrong.push(format!("{mode}: {r:?}: {}", text.trim()));
                }
            }
            drop(mounts);
            assert!(
                wrong.is_empty(),
                "the RUN's stdin is not /dev/null: {wrong:?}"
            );
        });
    }

    /// T2 (the VM only), pins the kernel behaviour that the `O_TMPFILE`
    /// copy, and a build that made no sealed copy, rely on (ADR-0101,
    /// threat-model section 6): a running binary cannot be opened for
    /// writing through `/proc/<pid>/exe` (`ETXTBSY`). Nothing is written:
    /// the open alone is refused.
    #[test]
    #[ignore = "T2: opens the running test binary for writing as root; run in the test VM only"]
    fn a_running_binary_cannot_be_opened_for_writing() {
        let exe = format!("/proc/{}/exe", std::process::id());
        let r = rustix::fs::open(
            exe.as_str(),
            OFlags::WRONLY | OFlags::CLOEXEC,
            Mode::empty(),
        );
        assert_eq!(r.err(), Some(Errno::TXTBSY), "{exe}");
    }

    /// Red team (T2, the VM only; T18): a `RUN --mount` target reached
    /// through symlinks that the bind itself changes (`/c -> /t/w`,
    /// `/t/w -> /t`, and the cache's own `w`) keeps the bind `nodev`, and
    /// read-only when asked, and no other mount of the sandbox changes:
    /// neither the root, nor the read-only `sysfs`, nor the read-only bind
    /// of the host's `/dev/null`. Both with `open_tree` and `move_mount` and
    /// as where they are refused (the symlinked target is then refused); in
    /// both, a control with a plain target and a secret runs, its bind,
    /// secret and `/proc/sys` read-only and `nodev`.
    #[test]
    #[ignore = "T2: mounts and mknod as real root in a private mount namespace; run in the test VM only"]
    fn red_team_a_symlinked_mount_target_keeps_its_bind_nodev() {
        in_private_mount_namespace(|| {
            use rustix::fs::StatVfsMountFlags;
            let mut mounts = TestMounts::new(tempfile::tempdir().unwrap());
            let top = mounts.path().canonicalize().unwrap();
            let st = rustix::fs::statvfs(&top).unwrap();
            assert!(
                !st.f_flag.contains(StatVfsMountFlags::NODEV),
                "the test directory's mount is nodev: the test would prove nothing"
            );
            let root = top.join("root");
            root_with_usr(&mut mounts, &root);
            fs::create_dir(root.join("t")).unwrap();
            std::os::unix::fs::symlink("/t", root.join("t/w")).unwrap();
            std::os::unix::fs::symlink("/t/w", root.join("c")).unwrap();
            fs::create_dir(root.join("d")).unwrap();
            fs::create_dir(root.join("e")).unwrap();
            let parent = ParentPid::new().unwrap();
            // The mount options of `mountpoint` in a `/proc/self/mountinfo`
            // listing.
            let options = |text: &str, mountpoint: &str| -> Option<String> {
                text.lines()
                    .map(|l| l.split(' ').collect::<Vec<_>>())
                    .find(|f| f.get(4) == Some(&mountpoint))
                    .and_then(|f| f.get(5).map(|o| (*o).to_owned()))
            };
            let has = |o: &Option<String>, flag: &str| {
                o.as_deref()
                    .is_some_and(|o| o.split(',').any(|f| f == flag))
            };
            let mut changed = Vec::new();
            let mut controls = Vec::new();
            for (n, mode) in ["mount API", "by path"].into_iter().enumerate() {
                let mut runner =
                    Runner::new(&top.join(format!("sandbox{n}")), Some(RootEntry::PivotRoot))
                        .unwrap();
                if mode == "by path" {
                    runner.without_mount_api();
                }
                let cache = top.join(format!("control-cache{n}"));
                fs::create_dir(&cache).unwrap();
                let mut spec = RunSpec::new(
                    &root,
                    vec![
                        "/bin/sh".into(),
                        "-c".into(),
                        "cat /proc/self/mountinfo; cat /run/secrets/s".into(),
                    ],
                );
                // One cache at two targets, as one key mounted twice: each
                // bind is remounted, neither in the other's place.
                for target in ["/d", "/e"] {
                    spec.mounts.push(crate::BindMount {
                        source: cache.clone(),
                        target: target.into(),
                        kind: MountKind::Directory,
                        read_only: true,
                    });
                }
                spec.secrets.push(SecretFile {
                    data: std::sync::Arc::from(&b"dummy-secret"[..]),
                    target: "/run/secrets/s".into(),
                    uid: 0,
                    gid: 0,
                    mode: 0o400,
                });
                let out = top.join(format!("control-out{n}"));
                output_to(&out, &mut spec);
                let r = runner.run(spec, None, |job| clone_child(job, parent));
                let text = fs::read_to_string(&out).unwrap();
                let (d, e, secret, procsys) = (
                    options(&text, "/d"),
                    options(&text, "/e"),
                    options(&text, "/run/secrets/s"),
                    options(&text, "/proc/sys"),
                );
                let what =
                    format!("{r:?}; /d {d:?}, /e {e:?}, secret {secret:?}, /proc/sys {procsys:?}");
                println!("control ({mode}): {what}");
                let ok = matches!(&r, Ok(st) if st.success())
                    && text.contains("dummy-secret")
                    && [&d, &e, &secret, &procsys]
                        .iter()
                        .all(|o| has(o, "ro") && has(o, "nodev"));
                controls.push((mode, ok, what));
                // (case, the cache's `w`, read-only, script)
                for (i, (case, link, read_only, script)) in [
                    (
                        "read-only cache",
                        "/",
                        true,
                        "cat /proc/self/mountinfo; mknod /t/made c 1 3 && echo x > /t/made && \
                     echo OPENED; touch /t/rw && echo WROTE",
                    ),
                    (
                        "read-write cache",
                        "/",
                        false,
                        "cat /proc/self/mountinfo; mknod /t/made c 1 3 && echo x > /t/made && \
                     echo OPENED",
                    ),
                    ("sysfs", "/sys", false, "cat /proc/self/mountinfo"),
                    ("/dev/null", "/dev/null", false, "cat /proc/self/mountinfo"),
                ]
                .into_iter()
                .enumerate()
                {
                    let case = format!("{case} ({mode})");
                    let cache = top.join(format!("cache{n}-{i}"));
                    fs::create_dir(&cache).unwrap();
                    std::os::unix::fs::symlink(link, cache.join("w")).unwrap();
                    let mut spec =
                        RunSpec::new(&root, vec!["/bin/sh".into(), "-c".into(), script.into()]);
                    spec.mounts.push(crate::BindMount {
                        source: cache,
                        target: "/c".into(),
                        kind: MountKind::Directory,
                        read_only,
                    });
                    let out = top.join(format!("out{n}-{i}"));
                    output_to(&out, &mut spec);
                    let r = runner.run(spec, None, |job| clone_child(job, parent));
                    let text = fs::read_to_string(&out).unwrap();
                    let (t, slash, sys, null) = (
                        options(&text, "/t"),
                        options(&text, "/"),
                        options(&text, "/sys"),
                        options(&text, "/dev/null"),
                    );
                    println!(
                        "{case}: {r:?}; /t {t:?}, / {slash:?}, /sys {sys:?}, /dev/null {null:?}\n{text}"
                    );
                    let rw = |o: &Option<String>| o.as_deref().is_some_and(|o| o.starts_with("rw"));
                    if text.contains("OPENED") || text.contains("WROTE") {
                        changed.push(format!("{case}: a node or file was written in /t"));
                    }
                    if t.is_some() && !has(&t, "nodev") {
                        changed.push(format!("{case}: /t is not nodev ({t:?})"));
                    }
                    if read_only && rw(&t) {
                        changed.push(format!("{case}: /t is writable ({t:?})"));
                    }
                    if rw(&sys)
                        || rw(&null)
                        || slash.as_deref().is_some_and(|o| o.starts_with("ro"))
                    {
                        changed.push(format!(
                        "{case}: another mount changed: / {slash:?}, /sys {sys:?}, /dev/null {null:?}"
                    ));
                    }
                }
            }
            drop(mounts);
            for (mode, ok, what) in &controls {
                assert!(ok, "the control RUN ({mode}): {what}");
            }
            assert!(
                changed.is_empty(),
                "a symlinked mount target misplaced the bind's remount: {changed:?}"
            );
        });
    }

    /// Red team (T2, the VM only; T7): a `RUN` with every kind of mount the
    /// child makes (a read-write and a read-only cache directory, a file
    /// bind, two secrets on the secrets tmpfs, `--network=none`) and stdin,
    /// stdout and stderr from files holds no descriptor but its stdio when
    /// it starts: the `open_tree` descriptors, the `proc`, `sysfs` and
    /// `/dev` descriptors, the mount sources, the parent check and the
    /// error pipe are all closed by `execve`. With `open_tree` and as where
    /// it is refused. `ls` itself opens one descriptor, the directory it
    /// lists, which is told apart by its target.
    #[test]
    #[ignore = "T2: mounts as real root in a private mount namespace; run in the test VM only"]
    fn red_team_a_run_with_every_mount_holds_only_its_stdio() {
        in_private_mount_namespace(|| {
            let mut mounts = TestMounts::new(tempfile::tempdir().unwrap());
            let top = mounts.path().canonicalize().unwrap();
            let root = top.join("root");
            root_with_usr(&mut mounts, &root);
            for d in ["c1", "c2"] {
                fs::create_dir(root.join(d)).unwrap();
            }
            fs::write(root.join("f"), b"").unwrap();
            let parent = ParentPid::new().unwrap();
            let mut leaked = Vec::new();
            for (n, mode) in ["mount API", "by path"].into_iter().enumerate() {
                let mut runner =
                    Runner::new(&top.join(format!("sandbox{n}")), Some(RootEntry::PivotRoot))
                        .unwrap();
                if mode == "by path" {
                    runner.without_mount_api();
                }
                let (c1, c2) = (
                    top.join(format!("cache{n}a")),
                    top.join(format!("cache{n}b")),
                );
                fs::create_dir(&c1).unwrap();
                fs::create_dir(&c2).unwrap();
                let file = top.join(format!("file{n}"));
                fs::write(&file, b"dummy").unwrap();
                let mut spec = RunSpec::new(
                    &root,
                    vec!["/bin/ls".into(), "-l".into(), "/proc/self/fd/".into()],
                );
                spec.network = Network::None;
                for (source, target, kind, read_only) in [
                    (c1, "/c1", MountKind::Directory, false),
                    (c2, "/c2", MountKind::Directory, true),
                    (file, "/f", MountKind::File, true),
                ] {
                    spec.mounts.push(crate::BindMount {
                        source,
                        target: target.into(),
                        kind,
                        read_only,
                    });
                }
                for target in ["/run/secrets/a", "/run/secrets/b"] {
                    spec.secrets.push(SecretFile {
                        data: std::sync::Arc::from(&b"dummy-secret"[..]),
                        target: target.into(),
                        uid: 0,
                        gid: 0,
                        mode: 0o400,
                    });
                }
                let out = top.join(format!("out{n}"));
                output_to(&out, &mut spec);
                let r = runner.run(spec, None, |job| clone_child(job, parent));
                let text = fs::read_to_string(&out).unwrap();
                println!("{mode}: {r:?}\n{text}");
                assert!(
                    matches!(&r, Ok(st) if st.success()),
                    "{mode}: {r:?}\n{text}"
                );
                for line in text.lines() {
                    let Some((left, target)) = line.split_once(" -> ") else {
                        continue;
                    };
                    let Some(fd) = left
                        .split_whitespace()
                        .last()
                        .and_then(|f| f.parse::<u32>().ok())
                    else {
                        continue;
                    };
                    // `ls`'s own: the directory it lists.
                    let own = target.starts_with("/proc/") && target.ends_with("/fd");
                    if fd > 2 && !own {
                        leaked.push(format!("{mode}: {line}"));
                    }
                }
            }
            drop(mounts);
            assert!(leaked.is_empty(), "descriptors a RUN holds: {leaked:?}");
        });
    }

    /// Red team (T2, the VM only; T18, threat model section 7): a root `RUN`
    /// cannot change the host's device nodes bound into its `/dev`: `chmod`
    /// and `chown` of each fail (read-only binds), `/dev/null` still
    /// writes, and the host's nodes keep their mode and owner.
    #[test]
    #[ignore = "T2: mounts as real root in a private mount namespace; run in the test VM only"]
    fn red_team_a_run_cannot_change_the_hosts_dev_nodes() {
        in_private_mount_namespace(|| {
            use std::os::unix::fs::MetadataExt as _;
            let nodes = ["null", "zero", "full", "random", "urandom", "tty"];
            let before: Vec<(u32, u32, u32)> = nodes
                .iter()
                .map(|n| {
                    let m = fs::metadata(format!("/dev/{n}")).unwrap();
                    (m.mode(), m.uid(), m.gid())
                })
                .collect();
            let mut mounts = TestMounts::new(tempfile::tempdir().unwrap());
            let top = mounts.path().canonicalize().unwrap();
            let root = top.join("root");
            root_with_usr(&mut mounts, &root);
            let script = nodes
                .iter()
                .map(|n| {
                    format!(
                        "chmod 0600 /dev/{n} && echo CHMOD-{n}; chown 1000:1000 /dev/{n} && \
                         echo CHOWN-{n}; "
                    )
                })
                .collect::<String>()
                + "echo x > /dev/null && echo NULL-WRITES";
            let mut spec = RunSpec::new(&root, vec!["/bin/sh".into(), "-c".into(), script.into()]);
            let out = top.join("out");
            output_to(&out, &mut spec);
            let runner = Runner::new(&top.join("sandbox"), Some(RootEntry::PivotRoot)).unwrap();
            let parent = ParentPid::new().unwrap();
            let r = runner.run(spec, None, |job| clone_child(job, parent));
            let text = fs::read_to_string(&out).unwrap();
            drop(mounts);
            let after: Vec<(u32, u32, u32)> = nodes
                .iter()
                .map(|n| {
                    let m = fs::metadata(format!("/dev/{n}")).unwrap();
                    (m.mode(), m.uid(), m.gid())
                })
                .collect();
            println!("{r:?}\n{text}");
            assert_eq!(before, after, "a host device node changed");
            assert!(
                !text.contains("CHMOD-") && !text.contains("CHOWN-"),
                "a RUN changed a /dev node: {text}"
            );
            assert!(text.contains("NULL-WRITES"), "control: {text}");
        });
    }

    /// SIGKILLs every process whose `comm` is `name`; how many there were.
    pub(super) fn kill_named(name: &str) -> usize {
        let want = format!("({name})");
        let mut killed = 0;
        for entry in fs::read_dir("/proc").unwrap().flatten() {
            let Ok(stat) = fs::read(entry.path().join("stat")) else {
                continue;
            };
            if !stat.windows(want.len()).any(|w| w == want.as_bytes()) {
                continue;
            }
            if let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|p| p.parse::<i32>().ok())
                .and_then(Pid::from_raw)
                && rustix::process::kill_process(pid, Signal::KILL).is_ok()
            {
                killed += 1;
            }
        }
        killed
    }

    /// T2, the VM only (ADR-0080): the command of a full-sandbox `RUN` is
    /// PID 2 of its namespace, under an init named `ranikust-init` that
    /// holds no capability, ignores what the command sends it, shows no
    /// argument or environment string of ranikust's in `/proc/1/cmdline`
    /// and keeps `/proc/1/environ` closed; ranikust sees the command's own
    /// status, an exit code or a signal, a set-up or `execve` failure of the
    /// command as before, and an end, by `SIGTRAP`, even for a command that
    /// made the init its tracer (`PTRACE_TRACEME`, as `USER 1000`).
    #[test]
    #[ignore = "T2: mounts as real root in a private mount namespace; run in the test VM only"]
    fn the_run_is_pid_2_under_an_init_that_reports_its_status() {
        in_private_mount_namespace(|| {
            let mut mounts = TestMounts::new(tempfile::tempdir().unwrap());
            let top = mounts.path().canonicalize().unwrap();
            let root = top.join("root");
            root_with_usr(&mut mounts, &root);
            let parent = ParentPid::new().unwrap();
            let runner = Runner::new(&top.join("sandbox"), Some(RootEntry::PivotRoot)).unwrap();
            let run_as = |user: &str, argv: Vec<std::ffi::OsString>, workdir: &str, out: &Path| {
                let mut spec = RunSpec::new(&root, argv);
                spec.user = user.into();
                spec.workdir = workdir.into();
                output_to(out, &mut spec);
                runner.run(spec, None, |job| clone_child(job, parent))
            };
            let run = |argv, workdir: &str, out: &Path| run_as("", argv, workdir, out);
            let sh = |script: &str| -> Vec<std::ffi::OsString> {
                vec!["/bin/sh".into(), "-c".into(), script.into()]
            };
            let out = top.join("layout.out");
            let status = run(
                sh("echo pid=$$; echo init=$(cat /proc/1/comm); \
                    grep -E '^Cap(Eff|Prm):' /proc/1/status; \
                    echo cmdline=$(tr -d '\\000' </proc/1/cmdline | wc -c); \
                    echo cmdline_bytes=$(wc -c </proc/1/cmdline); \
                    if cat /proc/1/environ >/dev/null 2>&1; \
                    then echo environ=readable; else echo environ=closed; fi; \
                    kill -KILL 1; kill -TERM 1; echo survived; exit 7"),
                "/",
                &out,
            )
            .unwrap();
            let text = fs::read_to_string(&out).unwrap();
            assert_eq!(status.code(), Some(7), "{text}");
            assert!(text.contains("pid=2\n"), "{text}");
            assert!(text.contains("init=ranikust-init\n"), "{text}");
            // ranikust's own command line (this test binary's) is not empty,
            // so an area of zeros proves the strings were cleared.
            assert!(text.contains("cmdline=0\n"), "{text}");
            assert!(!text.contains("cmdline_bytes=0\n"), "{text}");
            assert!(text.contains("environ=closed\n"), "{text}");
            assert!(
                text.contains("survived\n"),
                "the command killed its init: {text}"
            );
            for line in text.lines().filter(|l| l.starts_with("Cap")) {
                assert!(
                    line.split_whitespace()
                        .nth(1)
                        .is_some_and(|v| v.bytes().all(|b| b == b'0')),
                    "the init holds a capability: {line}"
                );
            }
            let status = run(sh("kill -TERM $$"), "/", &top.join("signal.out")).unwrap();
            assert_eq!(status.signal(), Some(libc::SIGTERM));
            // The command makes the init its tracer, then `execve`s: the
            // init's wait sees a stop, which is no end; it detaches and the
            // command ends, its stop signal handed on. As `USER 1000`: a
            // root command's permitted set is no subset of the init's empty
            // one, so its `PTRACE_TRACEME` fails and no stop happens.
            let traced = run_as(
                "1000:1000",
                vec![
                    "/usr/bin/python3".into(),
                    "-c".into(),
                    "import ctypes, os\n\
                     ctypes.CDLL(None).ptrace(0, 0, 0, 0)\n\
                     os.execv('/bin/sh', ['sh', '-c', 'exit 5'])\n"
                        .into(),
                ],
                "/",
                &top.join("traced.out"),
            )
            .unwrap();
            assert_eq!(traced.signal(), Some(libc::SIGTRAP), "{traced:?}");
            let r = run(vec!["/no/such/program".into()], "/", &top.join("exec.out"));
            assert!(matches!(r, Err(Error::Exec { .. })), "{r:?}");
            let r = run(sh("true"), "/no/such/dir", &top.join("workdir.out"));
            assert!(
                matches!(
                    r,
                    Err(Error::Setup {
                        step: Step::Workdir,
                        ..
                    })
                ),
                "{r:?}"
            );
        });
    }

    /// Red team (T2, the VM only; T8): no `RUN` outlives the thread that
    /// cloned it, the parent thread `PR_SET_PDEATHSIG` names, which in a
    /// build is the spawner thread and dies with ranikust (a second
    /// `SIGINT`, a `SIGKILL`, the OOM killer). Each `RUN` here is cloned on
    /// a thread that ends once the `RUN`'s shell has `exec`ed a copy of
    /// `sleep 300` under a name of its own; the `RUN` must then be killed,
    /// not left running in its namespaces. Cases: a plain binary as root
    /// (the control), as `USER 1000:1000` (the death signal armed again
    /// after `setresuid`, which clears it), as root through a copy that is
    /// setuid or setgid to 65534, whose `execve` changes the credentials
    /// and so clears the death signal again, after the child last armed
    /// it, and a `python3` that clears its own with
    /// `prctl(PR_SET_PDEATHSIG, 0)`, which needs no privilege. A `RUN`
    /// still running 15 s after its parent thread ended is killed by the
    /// test and counted.
    #[test]
    #[ignore = "T2: mounts, chown and setuid as real root in a private mount namespace; run in the test VM only"]
    fn red_team_no_run_outlives_the_thread_that_cloned_it() {
        in_private_mount_namespace(|| {
            use std::os::unix::fs::PermissionsExt as _;
            use std::time::{Duration, Instant};

            use rustix::fs::StatVfsMountFlags;
            let mut mounts = TestMounts::new(tempfile::tempdir().unwrap());
            let top = mounts.path().canonicalize().unwrap();
            let st = rustix::fs::statvfs(&top).unwrap();
            assert!(
                !st.f_flag.contains(StatVfsMountFlags::NOSUID),
                "the test directory's mount is nosuid: the test would prove nothing"
            );
            let root = top.join("root");
            root_with_usr(&mut mounts, &root);
            fs::create_dir(root.join("r")).unwrap();
            fs::set_permissions(root.join("r"), Permissions::from_mode(0o777)).unwrap();
            // (case, the sleeper's name, USER, its owner, its mode)
            let cases = [
                ("control: root, a plain binary", "rnkt8plain", "", 0, 0o755),
                (
                    "USER 1000:1000, a plain binary",
                    "rnkt8user",
                    "1000:1000",
                    0,
                    0o755,
                ),
                (
                    "root, a binary setuid to 65534",
                    "rnkt8suid",
                    "",
                    65534,
                    0o4755,
                ),
                (
                    "root, a binary setgid to 65534",
                    "rnkt8sgid",
                    "",
                    65534,
                    0o2755,
                ),
            ];
            for (_, name, _, owner, mode) in cases {
                let bin = root.join(name);
                fs::copy("/usr/bin/sleep", &bin).unwrap();
                let id = Some(rustix::fs::Uid::from_raw(owner));
                let gid = Some(rustix::fs::Gid::from_raw(owner));
                rustix::fs::chown(&bin, id, gid).unwrap();
                // After the `chown`, which clears the set-id bits.
                fs::set_permissions(&bin, Permissions::from_mode(mode)).unwrap();
            }
            // PID 1 of the `RUN` clears its own death signal, names itself,
            // then says it is ready.
            const PRCTL: &str = "import ctypes, time\n\
                libc = ctypes.CDLL(None)\n\
                libc.prctl(1, 0, 0, 0, 0)\n\
                libc.prctl(15, b'rnkt8prctl', 0, 0, 0)\n\
                open('/r/rnkt8prctl.ready', 'w').close()\n\
                time.sleep(300)\n";
            let cases = cases
                .map(|(case, name, user, _, _)| {
                    (
                        case,
                        name,
                        user,
                        format!("touch /r/{name}.ready; exec /{name} 300"),
                    )
                })
                .into_iter()
                .chain([(
                    "root, python3 clearing its own death signal",
                    "rnkt8prctl",
                    "",
                    format!("exec /usr/bin/python3 -c \"{PRCTL}\""),
                )]);
            let parent = ParentPid::new().unwrap();
            let mut outlived = Vec::new();
            let mut control_died = false;
            for (i, (case, name, user, script)) in cases.enumerate() {
                let runner =
                    Runner::new(&top.join(format!("sandbox{i}")), Some(RootEntry::PivotRoot))
                        .unwrap();
                let ready = root.join("r").join(format!("{name}.ready"));
                let mut spec =
                    RunSpec::new(&root, vec!["/bin/sh".into(), "-c".into(), script.into()]);
                spec.user = user.into();
                let out = top.join(format!("{name}.out"));
                output_to(&out, &mut spec);
                let (tx, rx) = std::sync::mpsc::channel();
                let start = Instant::now();
                let verdict = std::thread::scope(|s| {
                    s.spawn(|| {
                        let r = runner.run(spec, None, |job| {
                            let ready = ready.clone();
                            // The parent thread: it ends once the shell has
                            // replaced itself with the sleeper.
                            std::thread::spawn(move || {
                                let spawned = clone_child(job, parent);
                                let deadline = Instant::now() + Duration::from_secs(20);
                                while !ready.exists() && Instant::now() < deadline {
                                    std::thread::sleep(Duration::from_millis(10));
                                }
                                std::thread::sleep(Duration::from_millis(500));
                                spawned
                            })
                            .join()
                            .unwrap()
                        });
                        let _ = tx.send(r);
                    });
                    match rx.recv_timeout(Duration::from_secs(15)) {
                        Ok(r) => format!("ended after {:?}: {r:?}", start.elapsed()),
                        Err(_) => {
                            let killed = kill_named(name);
                            outlived
                                .push(format!("{case} ({killed} process(es) killed by the test)"));
                            format!("still running after {:?}", start.elapsed())
                        }
                    }
                });
                let text = fs::read_to_string(&out).unwrap_or_default();
                println!("{case}: {verdict}; output {text:?}");
                if i == 0 {
                    control_died = verdict.starts_with("ended");
                }
            }
            drop(mounts);
            assert!(
                control_died,
                "the control RUN outlived its parent thread: the test proves nothing"
            );
            assert!(
                outlived.is_empty(),
                "a RUN outlived the thread that cloned it: {outlived:?}"
            );
        });
    }

    /// The `RUN` script that makes its init `SCHED_IDLE` (`chrt -i -p 0 1`)
    /// and pins it to CPU 0 (`taskset -p 1 1`), both allowed to a process of
    /// the same uid without `CAP_SYS_NICE`; it writes the init's policy to
    /// `/r/policy`.
    pub(super) const IDLE_INIT: &str =
        "chrt -i -p 0 1 && taskset -p 1 1 >/dev/null && chrt -p 1 >/r/policy && ";

    /// A root for [`slow_init`] at `top/root`: the host's `/usr`, a
    /// world-writable `/r`, and copies of `yes` and `sleep` under names of
    /// their own.
    pub(super) fn spinner_root(mounts: &mut TestMounts, top: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let root = top.join("root");
        root_with_usr(mounts, &root);
        fs::create_dir(root.join("r")).unwrap();
        fs::set_permissions(root.join("r"), Permissions::from_mode(0o777)).unwrap();
        fs::copy("/usr/bin/yes", root.join("rnkspin")).unwrap();
        fs::copy("/usr/bin/sleep", root.join("rnkslow")).unwrap();
        root
    }

    /// How a [`slow_init`] `RUN` ended.
    pub(super) struct SlowInit {
        /// From the end of the thread that cloned the init to its reap,
        /// which follows the death of every process of the namespace.
        pub(super) delay: std::time::Duration,
        /// The `RUN`'s processes the test had to kill afterwards.
        pub(super) left: usize,
        pub(super) ready: bool,
        pub(super) report: String,
    }

    /// Runs `{frob}` then `spinners` copies of `yes` pinned to CPU 0 on
    /// `runner`, cloned on a thread that ends once they run, so that the
    /// init's death signal fires; case `i` names its files.
    pub(super) fn slow_init(
        runner: &Runner,
        top: &Path,
        root: &Path,
        i: usize,
        frob: &str,
        spinners: usize,
    ) -> SlowInit {
        use std::time::{Duration, Instant};
        let parent = ParentPid::new().unwrap();
        let ready = root.join("r").join(format!("ready{i}"));
        let script = format!(
            "{frob}n=0; while [ $n -lt {spinners} ]; do taskset -c 0 /rnkspin >/dev/null & \
             n=$((n + 1)); done; touch /r/ready{i}; exec /rnkslow 300"
        );
        let mut spec = RunSpec::new(root, vec!["/bin/sh".into(), "-c".into(), script.into()]);
        output_to(&top.join(format!("slow{i}.out")), &mut spec);
        let ended = std::sync::Mutex::new(None);
        let r = runner.run(spec, None, |job| {
            let ready = ready.clone();
            let spawned = std::thread::spawn(move || {
                let spawned = clone_child(job, parent);
                let deadline = Instant::now() + Duration::from_secs(20);
                while !ready.exists() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(10));
                }
                std::thread::sleep(Duration::from_millis(500));
                spawned
            })
            .join()
            .unwrap();
            *ended.lock().unwrap() = Some(Instant::now());
            spawned
        });
        let reaped = Instant::now();
        let delay = reaped - ended.lock().unwrap().expect("the cloning thread ended");
        let left = kill_named("rnkspin") + kill_named("rnkslow");
        let text = fs::read_to_string(top.join(format!("slow{i}.out"))).unwrap_or_default();
        SlowInit {
            delay,
            left,
            ready: ready.exists(),
            report: format!(
                "namespace gone {delay:?} after the cloning thread ended; {left} process(es) \
                 left; run: {r:?}; output {text:?}"
            ),
        }
    }

    /// Red team (T2, the VM only; T8, ADR-0080): a root `RUN` can slow its
    /// init's death, not stop it. The command makes the init `SCHED_IDLE`
    /// and pins it to CPU 0 ([`IDLE_INIT`]), then starts eight spinners
    /// pinned to CPU 0; once ready, the thread that cloned the init ends, so
    /// its death signal fires. The time from that thread's end to the
    /// init's reap is printed for this case and for a control without
    /// `chrt` and `taskset`; no process of either `RUN` may be left. The
    /// `RUN` has no cgroup of its own here; the nested variant is
    /// `cgroup_tests::a_nested_tree_shortens_the_spinner_cases_init_death`.
    #[test]
    #[ignore = "T2: mounts as real root in a private mount namespace; run in the test VM only"]
    fn red_team_a_run_can_slow_its_inits_death_but_not_stop_it() {
        in_private_mount_namespace(|| {
            use std::time::Duration;

            let mut mounts = TestMounts::new(tempfile::tempdir().unwrap());
            let top = mounts.path().canonicalize().unwrap();
            let root = spinner_root(&mut mounts, &top);
            let cases = [
                ("control", ""),
                ("init idle and pinned under 8 spinners", IDLE_INIT),
            ];
            let mut report = Vec::new();
            for (i, (case, frob)) in cases.into_iter().enumerate() {
                let runner =
                    Runner::new(&top.join(format!("sandbox{i}")), Some(RootEntry::PivotRoot))
                        .unwrap();
                let slow = slow_init(&runner, &top, &root, i, frob, 8);
                println!("{case}: {}", slow.report);
                report.push((case, slow));
            }
            let policy = fs::read_to_string(root.join("r/policy")).unwrap_or_default();
            println!("the init's policy as the RUN set it: {policy:?}");
            drop(mounts);
            assert!(
                policy.contains("SCHED_IDLE"),
                "the RUN could not make its init idle: the test proves nothing ({policy:?})"
            );
            for (case, slow) in report {
                assert!(slow.ready, "{case}: the RUN never got ready");
                assert_eq!(
                    slow.left, 0,
                    "{case}: a process of the RUN outlived its init"
                );
                assert!(
                    slow.delay < Duration::from_secs(60),
                    "{case}: the namespace took {:?} to die",
                    slow.delay
                );
            }
        });
    }

    /// A full-sandbox root whose `/proc`, `/sys` or `/dev` is a symlink is
    /// refused before any stub is placed or child cloned.
    #[test]
    fn a_symlinked_kernel_directory_in_the_root_is_refused() {
        for dir in ["proc", "sys", "dev"] {
            let tmp = tempfile::tempdir().unwrap();
            let top = tmp.path().canonicalize().unwrap();
            let root = top.join("root");
            fs::create_dir_all(root.join("elsewhere")).unwrap();
            std::os::unix::fs::symlink("/elsewhere", root.join(dir)).unwrap();
            let runner = Runner::new(&top.join("sandbox"), Some(RootEntry::PivotRoot)).unwrap();
            let spec = RunSpec::new(&root, vec!["/bin/true".into()]);
            let r = runner.run(spec, None, |_| panic!("no child may be cloned"));
            assert!(
                matches!(&r, Err(Error::InvalidSpec(m)) if m.contains("symbolic link")),
                "{dir}: {r:?}"
            );
            assert_eq!(fs::read_dir(top.join("sandbox/runs")).unwrap().count(), 0);
            assert_eq!(fs::read_dir(root.join("elsewhere")).unwrap().count(), 0);
        }
    }

    /// The probe of `open_tree` and `move_mount` answers the same every
    /// time and never crashes where the syscalls are missing.
    #[test]
    fn the_mount_api_is_probed_once() {
        let first = mount_api_supported();
        assert_eq!(first, mount_api_supported());
    }

    #[test]
    fn dev_nodes_are_remounted_read_only_and_never_nodev() {
        for flags in dev_remount() {
            assert!(flags.contains(MountFlags::BIND | MountFlags::RDONLY));
            assert!(!flags.contains(MountFlags::NODEV));
        }
    }

    #[test]
    fn sandbox_is_shareable_across_threads() {
        fn shareable<T: Send + Sync>() {}
        shareable::<Sandbox>();
    }

    #[test]
    fn ids_are_unique_within_a_runner() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Runner::new(dir.path(), None).unwrap();
        assert!(runner.runs_dir().is_dir());
        assert_ne!(runner.next_id(), runner.next_id());
    }

    /// Whether thread `tid` of this process is asleep within 60 s, as one
    /// waiting for its turn is; `false` once it has ended.
    fn asleep(tid: i32) -> bool {
        let stat = format!("/proc/self/task/{tid}/stat");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while std::time::Instant::now() < deadline {
            let Ok(s) = fs::read_to_string(&stat) else {
                return false;
            };
            // The state follows the name, which may hold a `)`.
            let state = s
                .rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().next());
            if state == Some("S") {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        false
    }

    /// Runs `call` on a detached thread, so that a turn never released fails
    /// the test instead of hanging it: the thread's id, and its result.
    fn detached<T: Send + 'static>(
        call: impl FnOnce() -> T + Send + 'static,
    ) -> (i32, mpsc::Receiver<T>) {
        let (ready, tid) = mpsc::channel();
        let (done, result) = mpsc::channel();
        std::thread::spawn(move || {
            ready
                .send(rustix::thread::gettid().as_raw_nonzero().get())
                .unwrap();
            // A test that already failed no longer listens.
            let _ = done.send(call());
        });
        (tid.recv().unwrap(), result)
    }

    /// A `Full` spec that [`RunSpec::validate`] refuses once its turn has
    /// come, before anything is cloned.
    fn empty_argv() -> RunSpec {
        RunSpec::new("/", Vec::new())
    }

    /// Within 60 s, why `result` was refused.
    #[track_caller]
    fn refusal(result: &mpsc::Receiver<Result<ExitStatus>>) -> String {
        match result.recv_timeout(std::time::Duration::from_secs(60)) {
            Ok(Err(Error::InvalidSpec(why))) => why,
            r => panic!("not a refusal: {r:?}"),
        }
    }

    /// The `RUN` on thread `tid` waits for its turn.
    #[track_caller]
    fn assert_waits((tid, result): &(i32, mpsc::Receiver<Result<ExitStatus>>)) {
        assert!(
            asleep(*tid),
            "a RUN that should wait: {:?}",
            result.try_recv()
        );
        assert!(result.try_recv().is_err(), "a RUN that should wait");
    }

    /// Under an rbind of `/proc` a `RUN` runs alone, whichever `Sandbox`
    /// runs it: it waits for every `RUN` of the process, every `RUN` waits
    /// for it, and a `Direct` one is refused without waiting. With a new
    /// `proc`, `RUN`s do not wait for each other. The T1
    /// `runs_take_turns_under_an_rbind_of_proc` checks that a turn lasts
    /// until its `RUN` is reaped.
    #[test]
    fn rbind_runs_run_alone_and_the_others_side_by_side() {
        let empty = tempfile::tempdir().unwrap();
        let missing = empty.path().canonicalize().unwrap().join("missing");
        let dirs = [(); 2].map(|()| tempfile::tempdir().unwrap());
        static OWN_TURNS: Turns = Turns::new();
        let turns = &OWN_TURNS;
        // Without `Sandbox::start`'s cgroups and profile. Kept here, so that a
        // waiting thread's only sleep is its turn, not the drop of its `Sandbox`.
        let [fresh, rbind] =
            [(&dirs[0], KernelFs::Fresh), (&dirs[1], KernelFs::Rbind)].map(|(dir, proc)| {
                let runner = Runner::new(dir.path(), Some(RootEntry::PivotRoot))
                    .unwrap()
                    .with_kernel_fs(proc, KernelFs::Fresh);
                let mut sandbox = Sandbox::spawn_thread(runner).unwrap();
                assert!(std::ptr::eq(sandbox.turns, &TURNS));
                sandbox.turns = turns;
                std::sync::Arc::new(sandbox)
            });
        let run = |sandbox: &std::sync::Arc<Sandbox>, spec: RunSpec| {
            let sandbox = sandbox.clone();
            detached(move || sandbox.run(spec))
        };

        assert!(matches!(
            turns.take(KernelFs::Rbind, Isolation::Direct),
            Err(Error::InvalidSpec(_))
        ));
        let shared = turns.take(KernelFs::Fresh, Isolation::Full).unwrap();
        let side_by_side = refusal(&run(&fresh, empty_argv()).1);
        assert!(side_by_side.contains("argv is empty"), "{side_by_side}");
        let alone = run(&rbind, empty_argv());
        assert_waits(&alone);
        drop(shared);
        let after = refusal(&alone.1);
        assert!(after.contains("argv is empty"), "{after}");

        let held = turns.take(KernelFs::Rbind, Isolation::Full).unwrap();
        let direct = refusal(&run(&rbind, direct(&missing, &missing)).1);
        assert!(direct.contains("rbind of /proc"), "{direct}");
        let waiting = [run(&fresh, empty_argv()), run(&rbind, empty_argv())];
        waiting.iter().for_each(assert_waits);
        drop(held);
        for (_, result) in &waiting {
            let after = refusal(result);
            assert!(after.contains("argv is empty"), "{after}");
        }
    }

    /// A panic during a turn, alone or shared, refuses every later turn: the
    /// `RUN`'s processes may have outlived it.
    #[test]
    fn a_panic_during_a_turn_refuses_every_later_one() {
        let kinds = [
            (KernelFs::Rbind, Isolation::Full),
            (KernelFs::Fresh, Isolation::Full),
            (KernelFs::Fresh, Isolation::Direct),
        ];
        for (proc, isolation) in kinds {
            let turns = Turns::new();
            std::thread::scope(|s| {
                let panicked = s.spawn(|| {
                    let _turn = turns.take(proc, isolation).unwrap();
                    panic!("a RUN that panics during its turn");
                });
                assert!(panicked.join().is_err());
            });
            for (later, how) in kinds {
                assert!(
                    matches!(turns.take(later, how), Err(Error::TurnPoisoned)),
                    "{later:?} {how:?} after a panic in {proc:?} {isolation:?}"
                );
            }
        }
    }

    /// A spawner thread lost during a turn, which may have cloned the
    /// `RUN`, refuses every later turn.
    #[test]
    fn a_spawner_lost_during_a_turn_refuses_every_later_one() {
        let empty = tempfile::tempdir().unwrap();
        let missing = empty.path().canonicalize().unwrap().join("missing");
        let dir = tempfile::tempdir().unwrap();
        let runner = Runner::new(dir.path(), Some(RootEntry::PivotRoot)).unwrap();
        let (requests, received) = mpsc::channel::<Request>();
        // Takes the job and drops it unanswered, as a spawner that died.
        let lost = std::thread::spawn(move || drop(received.recv()));
        static OWN_TURNS: Turns = Turns::new();
        let turns = &OWN_TURNS;
        let sandbox = Sandbox {
            runner,
            running: Running::default(),
            requests: Some(requests),
            thread: Some(lost),
            turns,
        };
        match sandbox.run(direct(&missing, &missing)) {
            Err(Error::SpawnerGone) => {}
            r => panic!("a RUN whose spawner is lost: {r:?}"),
        }
        for (proc, isolation) in [
            (KernelFs::Rbind, Isolation::Full),
            (KernelFs::Fresh, Isolation::Full),
            (KernelFs::Fresh, Isolation::Direct),
        ] {
            assert!(
                matches!(turns.take(proc, isolation), Err(Error::TurnPoisoned)),
                "{proc:?} {isolation:?}"
            );
        }
    }

    /// Where `RUN`s get an rbind of `/proc`, a `Direct` one is refused
    /// before its clone, whatever the root entry; with a new `proc` it gets
    /// as far as the clone.
    #[test]
    fn direct_is_refused_where_runs_get_an_rbind_of_proc() {
        let empty = tempfile::tempdir().unwrap();
        let missing = empty.path().canonicalize().unwrap().join("missing");
        let cases = [Some(RootEntry::PivotRoot), None]
            .into_iter()
            .flat_map(|entry| [KernelFs::Rbind, KernelFs::Fresh].map(|proc| (entry, proc)));
        for (entry, proc) in cases {
            let dir = tempfile::tempdir().unwrap();
            let runner = Runner::new(dir.path(), entry)
                .unwrap()
                .with_kernel_fs(proc, KernelFs::Fresh);
            let r = runner.run(direct(&missing, &missing), None, |_| {
                Err(Error::SpawnerGone)
            });
            match (proc, r) {
                (KernelFs::Rbind, Err(Error::InvalidSpec(why))) => {
                    assert!(why.contains("rbind of /proc"), "{why}");
                }
                (KernelFs::Fresh, Err(Error::SpawnerGone)) => {}
                (proc, r) => panic!("{entry:?} {proc:?}: {r:?}"),
            }
        }
    }
}
