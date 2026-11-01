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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError, mpsc};
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

use rnk_rootfs::{RootPath, User};
use rustix::fs::{FileType, Mode, OFlags};
use rustix::io::Errno;
use rustix::mount::MountFlags;
use rustix::pipe::PipeFlags;
use rustix::process::{Gid, Pid, Signal, Uid, WaitId, WaitIdOptions};
use tracing::Span;
use tracing::field::Empty;

use crate::caps;
use crate::child::{
    self, ChildArgs, ChildFds, ChildMount, ChildSecret, ChildSecrets, ChildStdio, ExecPtrs,
    RootEntry,
};
use crate::direct;
use crate::error::{Error, Result, Step, io_err, sys_err};
use crate::journal::{self, Content, Journal};
use crate::plan::{self, DEV_NODES, EtcFiles, MountOp};
use crate::probe::Caps;
use crate::relay::Relay;
use crate::seccomp;
use crate::spec::{DEFAULT_HOSTNAME, Isolation, MountKind, Network, RunSpec, SecretFile, Stdio};
use crate::sweep;

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
}

impl Sandbox {
    /// Replays the stub journals that dead builds left in `dir/runs`, then
    /// starts the spawner thread. `dir` is the sandbox's directory in the
    /// state directory, `<state-dir>/sandbox`; `caps` is what [`crate::probe`]
    /// found. Such a sandbox runs [`Isolation::Full`] on any root, and
    /// [`Isolation::Direct`] on `/` only: a bare `chroot` is never used where
    /// the full sandbox can run (ADR-0014, ADR-0060). Kaniko mode that
    /// cannot mount uses [`Sandbox::start_direct`].
    ///
    /// # Errors
    ///
    /// The directory cannot be created or is a symlink, a journal cannot be
    /// listed, or the thread cannot start.
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
        let runner = Runner::new(dir, caps.map(|c| c.root_entry))?;
        let replayed = journal::replay(&runner.runs_dir, &runner.state_dir, shared_root)?;
        if replayed.journals > 0 {
            tracing::info!(
                "rnk-sandbox: removed {} stub(s) left by {} interrupted RUN(s)",
                replayed.stubs,
                replayed.journals
            );
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
        })
    }

    /// Runs one `RUN` and waits for it; the exit status is the command's.
    ///
    /// Blocking: an async caller runs it on a blocking thread. The child is
    /// cloned on the spawner thread whatever thread calls this.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidSpec`], [`Error::Rootfs`] for the `USER` lookup or a
    /// stub, [`Error::Setup`] with the failing step and errno, [`Error::Exec`]
    /// when the command cannot be executed, [`Error::Privilege`] for
    /// [`Isolation::Full`] without `CAP_SYS_ADMIN` or without [`Caps`],
    /// [`Error::Cancelled`] after [`Sandbox::cancel`], or a host-side error.
    pub fn run(&self, spec: RunSpec) -> Result<ExitStatus> {
        let requests = self.requests.as_ref().ok_or(Error::SpawnerGone)?;
        self.runner.run(spec, Some(&self.running), |job| {
            let (reply, answer) = mpsc::sync_channel(1);
            requests
                .send(Request { job, reply })
                .map_err(|_| Error::SpawnerGone)?;
            answer.recv().map_err(|_| Error::SpawnerGone)?
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

    /// Every later `RUN`, of any isolation, starts with `limit` as its
    /// `RLIMIT_NOFILE`: the limit ranikust started with, captured with
    /// [`NofileLimit::current`](crate::NofileLimit::current) before it
    /// raised its own for the kaniko-mode walk. Without it a `RUN` inherits
    /// ranikust's limit. Call before the first `RUN`.
    pub fn set_nofile_limit(&mut self, limit: crate::NofileLimit) {
        self.runner.nofile = Some(limit);
    }

    /// Cancels the build's `RUN`s: kills every one running with `SIGKILL`,
    /// as BuildKit does on a cancel, and makes every later [`Sandbox::run`]
    /// fail with [`Error::Cancelled`]. Stubs are cleaned up as after any
    /// `RUN`, before `run` returns. Any thread may call it.
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

/// `fd`, or a close-on-exec duplicate numbered 3 or higher when it is 0, 1
/// or 2 (ranikust started with a standard stream closed): the child's
/// `dup2` onto 0–2 must not overwrite it.
fn above_stdio(fd: OwnedFd) -> Result<OwnedFd> {
    if fd.as_raw_fd() >= 3 {
        return Ok(fd);
    }
    rustix::io::fcntl_dupfd_cloexec(&fd, 3).map_err(|e| sys_err("fcntl(F_DUPFD_CLOEXEC)", e))
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
    } = job;
    let (failures, err_w) =
        rustix::pipe::pipe_with(PipeFlags::CLOEXEC).map_err(|e| sys_err("pipe2", e))?;
    let err_w = above_stdio(err_w)?;
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
                Some((_, errno)) => Err(io::Error::from_raw_os_error(errno)),
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
    drops: ([u32; 64], usize),
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
}

/// What identifies a root directory: device, inode and mount id.
type RootId = (u32, u32, u64, u64);

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

fn root_id(root: &RootPath) -> Result<RootId> {
    let st = root.statx("/")?;
    Ok((st.dev_major, st.dev_minor, st.ino, st.mnt_id))
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
            drops: caps::bounding_drops(caps::last_cap()),
            next: AtomicU64::new(0),
            net_mode: OnceLock::new(),
            userns_network: false,
            nofile: None,
            shared_root: None,
            direct_only: false,
        })
    }

    /// A runner for `Direct` on `/` only: no directory, no journal, nothing
    /// to replay (ADR-0051).
    fn direct_only() -> Self {
        Self {
            runs_dir: PathBuf::new(),
            state_dir: PathBuf::new(),
            root_entry: None,
            drops: caps::bounding_drops(caps::last_cap()),
            next: AtomicU64::new(0),
            net_mode: OnceLock::new(),
            userns_network: false,
            nofile: None,
            shared_root: None,
            direct_only: true,
        }
    }

    pub(crate) fn runs_dir(&self) -> &Path {
        &self.runs_dir
    }

    pub(crate) fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// Makes `--network=none` use a [`NetNs`] whatever the probe would
    /// find: the path of an environment without `CAP_NET_ADMIN`, tested
    /// where the process has it.
    #[cfg(test)]
    pub(crate) fn hold_networks(&self) {
        let _ = self.net_mode.set(NetMode::UserNs);
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
            // Unused by `Direct`.
            (_, entry) => entry.unwrap_or(RootEntry::PivotRoot),
        };
        let program = spec.argv[0].to_string_lossy().into_owned();
        let setup = if traced {
            tracing::debug_span!("rnk_sandbox.setup", stubs = Empty)
        } else {
            Span::none()
        };
        let (mut job, journal) = setup.in_scope(|| self.prepare(&mut spec, root_entry))?;
        setup.record("stubs", journal.as_ref().map_or(0, Journal::stub_count));
        let relay = match Relay::start(
            &mut job.args.stdio,
            &mut job.fds,
            job.args.uid,
            job.args.gid,
        ) {
            Ok(relay) => relay,
            Err(e) => {
                if let Some(j) = journal
                    && let Err(c) = j.finish()
                {
                    tracing::warn!("rnk-sandbox: stub clean-up failed: {c}");
                }
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
        let result = span.in_scope(|| execute(job, spawn, &program, running, self.direct_only));
        match &result {
            Ok((status, usage)) => {
                record_usage(&span, usage);
                span.record("failed", !status.success());
            }
            Err(_) => {
                span.record("failed", true);
            }
        }
        drop(span);
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
            let mounts = plan::mount_plan(spec, &files);
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
        // In a `chroot`, `/dev/null` is the image's entry, or missing where
        // `mknod` is refused: the host's is opened here instead.
        let stdio = if chroot.is_some() {
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
            child_stdio(stdin, &mut keep)?,
            child_stdio(stdout, &mut keep)?,
            child_stdio(stderr, &mut keep)?,
        ];
        let mounts = mounts.into_iter().map(child_mount).collect::<Result<_>>()?;
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
        // A `RUN` in a user namespace's network namespace never runs
        // without its filter.
        let seccomp = if hold_network {
            Some(
                seccomp::userns_network().ok_or_else(|| Error::NoLoopbackUserns {
                    direct: io::Error::from_raw_os_error(libc::EPERM),
                    userns: io::Error::from_raw_os_error(libc::ENOSYS),
                })?,
            )
        } else {
            None
        };
        let args = ChildArgs {
            root_dir: cstring(&spec.root_dir)?,
            root_remount,
            root_nodev_rec,
            mounts,
            secrets,
            dev_remount,
            dev_pipes: plan::dev_pipes(spec),
            root_entry,
            hostname: hostname(spec).as_bytes().to_vec(),
            loopback,
            netns: None,
            // Only with a `NetNs`; `probe_network` checked it exists.
            seccomp,
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
        };
        Ok(Job {
            args,
            flags: libc::c_long::from(flags),
            isolation: spec.isolation,
            hold_network,
            fds: keep,
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

/// The host's `/dev/null`, read-write, close-on-exec.
fn host_null() -> Result<OwnedFd> {
    let p = Path::new("/dev/null");
    rustix::fs::open(p, OFlags::RDWR | OFlags::CLOEXEC, Mode::empty())
        .map_err(|e| io_err("open", p, e.into()))
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

fn child_mount(op: MountOp) -> Result<ChildMount> {
    Ok(match op {
        MountOp::Proc { target } => ChildMount::Proc {
            target: cstring(&target)?,
        },
        MountOp::Sysfs { target } => ChildMount::Sysfs {
            target: cstring(&target)?,
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

/// A caller's stream for the child: a descriptor is duplicated to 3 or
/// higher, close-on-exec, and kept open in `keep` until the clone.
fn child_stdio(s: Stdio, keep: &mut Vec<OwnedFd>) -> Result<ChildStdio> {
    Ok(match s {
        Stdio::Inherit => ChildStdio::Inherit,
        Stdio::Null => ChildStdio::Null,
        Stdio::Fd(fd) => {
            let dup = rustix::io::fcntl_dupfd_cloexec(&fd, 3)
                .map_err(|e| sys_err("duplicate a stdio descriptor", e))?;
            let raw: RawFd = dup.as_raw_fd();
            keep.push(dup);
            ChildStdio::Fd(raw)
        }
    })
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
        let (uid, gid) = (Uid::from_raw(s.uid), Gid::from_raw(s.gid));
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

/// The `RUN`s running now, for [`Sandbox::cancel`].
#[derive(Debug, Default)]
pub(crate) struct Running(Mutex<RunningState>);

#[derive(Debug, Default)]
struct RunningState {
    cancelled: bool,
    pids: Vec<(Pid, Isolation)>,
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
        for &(pid, isolation) in &state.pids {
            kill_run(pid, isolation);
        }
    }

    fn is_cancelled(&self) -> bool {
        self.state().cancelled
    }

    /// Records a cloned child; kills it at once after a cancel.
    fn add(&self, pid: Pid, isolation: Isolation) {
        let mut state = self.state();
        state.pids.push((pid, isolation));
        if state.cancelled {
            kill_run(pid, isolation);
        }
    }

    /// Forgets a child that has exited and is not reaped yet, so that its pid
    /// cannot have been reused; returns whether the build was cancelled.
    fn remove(&self, pid: Pid) -> bool {
        let mut state = self.state();
        state.pids.retain(|&(p, _)| p != pid);
        state.cancelled
    }
}

/// `SIGKILL` to a `RUN`, as BuildKit kills the process on a cancel
/// (`executor/runcexecutor/executor.go`). A `Full` child is PID 1 of its
/// namespace, so the kernel kills the rest; a `Direct` one leads its process
/// group. Called only while the child is not reaped, so the pid is its own;
/// `ESRCH` (already gone) is no error.
fn kill_run(pid: Pid, isolation: Isolation) {
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
/// `running`, the child can be killed by [`Sandbox::cancel`] until it exits.
/// With `sweep` (a [`Sandbox::start_direct`] sandbox: a subreaper running
/// one `RUN` at a time), what the `RUN` left is killed and reaped too.
fn execute(
    job: Job,
    spawn: impl FnOnce(Job) -> Result<Spawned>,
    program: &str,
    running: Option<&Running>,
    sweep: bool,
) -> Result<(ExitStatus, libc::rusage)> {
    let isolation = job.isolation;
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
        netns,
    } = spawn(job)?;
    if let Some(r) = running {
        r.add(pid, isolation);
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
            kill_run(pid, isolation);
            wait_exited(pid).map_err(|_| e)
        }
    });
    let cancelled = running.is_some_and(|r| r.remove(pid));
    let killed = match (exited, isolation) {
        (Ok(()), Isolation::Direct) if sweep => {
            kill_leftovers(pid).and_then(|()| sweep::kill_run_tree(pid, &helpers))
        }
        (Ok(()), Isolation::Direct) => kill_leftovers(pid),
        // The child is PID 1 of its namespace: when it exits, the kernel
        // kills the rest.
        (r, _) => r,
    };
    let reaped = reap(pid);
    // The child is reaped: its namespace can end with this descriptor.
    drop(netns);
    let (status, usage) = reaped?;
    killed?;
    if cancelled {
        return Err(Error::Cancelled);
    }
    // `Setup` and `Exec` come from the child before `execve` and are
    // returned only here, after the tree is reaped and swept (`killed?`):
    // `Error::ran_no_command` relies on it (ADR-0056). Keep it so.
    match failure? {
        None => Ok((status, usage)),
        Some((Step::Exec, errno)) => Err(Error::Exec {
            program: program.to_owned(),
            source: io::Error::from_raw_os_error(errno),
        }),
        Some((step, errno)) => Err(Error::Setup {
            step,
            source: io::Error::from_raw_os_error(errno),
        }),
    }
}

/// The child's failure record, or `None` at end-of-file (it executed the
/// command, which closed the pipe).
fn read_failure(fd: &OwnedFd) -> Result<Option<(Step, i32)>> {
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
    #[ignore = "T2: creates network namespaces; run as root in the test VM only"]
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
        let rec = child::encode_failure(Step::MountProc, libc::EPERM);
        rustix::io::write(&w, &rec).unwrap();
        drop(w);
        assert_eq!(
            read_failure(&r).unwrap(),
            Some((Step::MountProc, libc::EPERM))
        );

        let (r, w) = rustix::pipe::pipe_with(PipeFlags::CLOEXEC).unwrap();
        rustix::io::write(&w, &rec[..3]).unwrap();
        drop(w);
        assert!(read_failure(&r).is_err());
    }

    #[test]
    fn stdio_descriptors_are_moved_above_two() {
        let mut keep = Vec::new();
        let (r, _w) = rustix::pipe::pipe_with(PipeFlags::CLOEXEC).unwrap();
        let s = child_stdio(Stdio::Fd(r), &mut keep).unwrap();
        let ChildStdio::Fd(fd) = s else {
            panic!("{s:?}")
        };
        assert!(fd >= 3);
        assert_eq!(keep.len(), 1);
        assert_eq!(
            child_stdio(Stdio::Null, &mut keep).unwrap(),
            ChildStdio::Null
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
        running.add(pid, Isolation::Direct);
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
        running.add(pid, Isolation::Full);
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
    /// runs in the T2 VM, on the VM's own `/bin/sleep`.
    #[test]
    #[ignore = "T2: a Direct RUN switches groups; run as root in the test VM only"]
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
    /// (the `RUN` switches to root's ids); T2.
    #[test]
    #[ignore = "T2: a Direct RUN switches groups; run as root in the test VM only"]
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
    /// (kaniko#2928). T2.
    #[test]
    #[ignore = "T2: a Direct RUN switches groups; run as root in the test VM only"]
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
        let m = child_mount(MountOp::Bind {
            source: d.join("cache"),
            target: "/root/.cache".into(),
            kind: MountKind::Directory,
            read_only: true,
        })
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
    struct TestMounts {
        dir: Option<tempfile::TempDir>,
        targets: Vec<PathBuf>,
    }

    impl TestMounts {
        fn new(dir: tempfile::TempDir) -> Self {
            Self {
                dir: Some(dir),
                targets: Vec::new(),
            }
        }

        fn path(&self) -> &Path {
            self.dir
                .as_ref()
                .map_or(Path::new("/nonexistent"), |d| d.path())
        }

        /// Records a mount just made, right after it succeeded.
        fn add(&mut self, target: PathBuf) {
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
}
