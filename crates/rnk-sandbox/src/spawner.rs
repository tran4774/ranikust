//! The parent side of a `RUN`: preparing the child's arguments and stubs,
//! the spawner thread that clones every child, reading the failure record,
//! reaping with `wait4` and its resource usage.

use std::ffi::{CString, OsStr};
use std::fs::{self, File, Permissions};
use std::io::{self, Write as _};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

use rnk_rootfs::{RootPath, User};
use rustix::fs::{FileType, Mode, OFlags};
use rustix::io::Errno;
use rustix::mount::MountFlags;
use rustix::pipe::PipeFlags;
use rustix::process::{Gid, Pid, PidfdFlags, Signal, Uid, WaitId, WaitIdOptions};
use tracing::Span;
use tracing::field::Empty;

use crate::caps;
use crate::child::{self, ChildArgs, ChildFds, ChildMount, ChildStdio, ExecPtrs, RootEntry};
use crate::direct;
use crate::error::{Error, Result, Step, io_err, sys_err};
use crate::journal::{self, Journal};
use crate::plan::{self, DEV_NODES, EtcFiles, MountOp};
use crate::probe::Caps;
use crate::spec::{DEFAULT_HOSTNAME, Isolation, MountKind, Network, RunSpec, Stdio};

/// Runs `RUN` commands.
///
/// Owns the spawner thread every child is cloned from: `PR_SET_PDEATHSIG`
/// fires when the parent *thread* exits, so that thread lives as long as the
/// `Sandbox`, and dropping the `Sandbox` kills every `RUN` still running.
#[derive(Debug)]
pub struct Sandbox {
    runner: Runner,
    requests: Option<mpsc::Sender<Request>>,
    thread: Option<JoinHandle<()>>,
}

impl Sandbox {
    /// Replays the stub journals that dead builds left in `dir/runs`, then
    /// starts the spawner thread. `dir` is the sandbox's directory in the
    /// state directory, `<state-dir>/sandbox`; `caps` is what [`crate::probe`]
    /// found, `None` where it failed: kaniko mode that cannot mount, which
    /// runs [`Isolation::Direct`] only.
    ///
    /// # Errors
    ///
    /// The directory cannot be created, a journal cannot be listed, or the
    /// thread cannot start.
    pub fn start(caps: Option<Caps>, dir: &Path) -> Result<Self> {
        let runner = Runner::new(dir, caps.map(|c| c.root_entry))?;
        let replayed = journal::replay(&runner.runs_dir)?;
        if replayed.journals > 0 {
            tracing::info!(
                "rnk-sandbox: removed {} stub(s) left by {} interrupted RUN(s)",
                replayed.stubs,
                replayed.journals
            );
        }
        let parent = ParentFd::new()?;
        let (requests, received) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("rnk-sandbox-spawner".into())
            .spawn(move || spawner(&received, &parent))
            .map_err(|e| sys_err("start the spawner thread", e))?;
        Ok(Self {
            runner,
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
    /// [`Isolation::Full`] without `CAP_SYS_ADMIN` or without [`Caps`], or a
    /// host-side error.
    pub fn run(&self, spec: RunSpec) -> Result<ExitStatus> {
        let requests = self.requests.as_ref().ok_or(Error::SpawnerGone)?;
        self.runner.run(spec, true, |job| {
            let (reply, answer) = mpsc::sync_channel(1);
            requests
                .send(Request { job, reply })
                .map_err(|_| Error::SpawnerGone)?;
            answer.recv().map_err(|_| Error::SpawnerGone)?
        })
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

/// A prepared child, to be cloned.
pub(crate) struct Job {
    args: ChildArgs,
    /// `clone` flags, the exit signal included.
    flags: libc::c_long,
    isolation: Isolation,
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
}

/// A pidfd of this process, which a child polls after arming
/// `PR_SET_PDEATHSIG` to learn whether the process died before: it becomes
/// readable once the process has exited, whoever else holds a copy.
#[derive(Debug)]
pub(crate) struct ParentFd(OwnedFd);

impl ParentFd {
    pub(crate) fn new() -> Result<Self> {
        let fd = rustix::process::pidfd_open(rustix::process::getpid(), PidfdFlags::empty())
            .map_err(|e| sys_err("pidfd_open", e))?;
        Ok(Self(above_stdio(fd)?))
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

fn spawner(requests: &mpsc::Receiver<Request>, parent: &ParentFd) {
    for Request { job, reply } in requests {
        let spawned = clone_child(job, parent);
        // The caller waits on the answer; it can only be gone if it
        // panicked, which aborts the process.
        let _ = reply.send(spawned);
    }
}

/// Clones `job` on the calling thread, which becomes the child's parent
/// thread for `PR_SET_PDEATHSIG`.
pub(crate) fn clone_child(job: Job, parent: &ParentFd) -> Result<Spawned> {
    let Job {
        args,
        flags,
        isolation,
        fds: keep,
    } = job;
    let (failures, err_w) =
        rustix::pipe::pipe_with(PipeFlags::CLOEXEC).map_err(|e| sys_err("pipe2", e))?;
    let err_w = above_stdio(err_w)?;
    let ptrs = ExecPtrs::new(&args);
    let fds = ChildFds {
        err_w: err_w.as_raw_fd(),
        parent: parent.0.as_raw_fd(),
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
    Ok(Spawned { pid, failures })
}

/// Prepares, runs and cleans up `RUN`s; the cloning is the caller's.
#[derive(Debug)]
pub(crate) struct Runner {
    runs_dir: PathBuf,
    /// `None`: the probe failed, only [`Isolation::Direct`] runs.
    root_entry: Option<RootEntry>,
    drops: ([u32; 64], usize),
    next: AtomicU64,
}

impl Runner {
    /// A runner keeping its journals in `dir/runs`, created if missing.
    pub(crate) fn new(dir: &Path, root_entry: Option<RootEntry>) -> Result<Self> {
        let runs_dir = dir.join("runs");
        fs::create_dir_all(&runs_dir).map_err(|e| io_err("create", &runs_dir, e))?;
        Ok(Self {
            runs_dir,
            root_entry,
            drops: caps::bounding_drops(caps::last_cap()),
            next: AtomicU64::new(0),
        })
    }

    pub(crate) fn runs_dir(&self) -> &Path {
        &self.runs_dir
    }

    /// Runs `spec`, cloning through `spawn`. With `traced`, records the
    /// `rnk_sandbox.setup` and `rnk_sandbox.run` spans.
    pub(crate) fn run(
        &self,
        mut spec: RunSpec,
        traced: bool,
        spawn: impl FnOnce(Job) -> Result<Spawned>,
    ) -> Result<ExitStatus> {
        spec.validate()?;
        let root_entry = match (spec.isolation, self.root_entry) {
            (Isolation::Full, None) => return Err(Error::Privilege("the RUN sandbox")),
            // Unused by `Direct`.
            (_, entry) => entry.unwrap_or(RootEntry::PivotRoot),
        };
        let program = spec.argv[0].to_string_lossy().into_owned();
        let setup = if traced {
            tracing::debug_span!("rnk_sandbox.setup", stubs = Empty)
        } else {
            Span::none()
        };
        let (job, journal) = setup.in_scope(|| self.prepare(&mut spec, root_entry))?;
        setup.record("stubs", journal.as_ref().map_or(0, Journal::stub_count));
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
        let result = span.in_scope(|| execute(job, spawn, &program));
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
        let user = rnk_rootfs::lookup_user(&root, spec.user_spec())?;
        let stdio = [
            std::mem::take(&mut spec.stdin),
            std::mem::take(&mut spec.stdout),
            std::mem::take(&mut spec.stderr),
        ];
        if direct {
            return Ok((self.job(spec, &user, Vec::new(), stdio, root_entry)?, None));
        }
        let mut journal = Journal::create(&self.runs_dir, &self.next_id(), &spec.root_dir, root)?;
        match place(&mut journal, spec).and_then(|files| {
            let mounts = plan::mount_plan(spec, &files);
            self.job(spec, &user, mounts, stdio, root_entry)
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

    fn job(
        &self,
        spec: &RunSpec,
        user: &User,
        mounts: Vec<MountOp>,
        stdio: [Stdio; 3],
        root_entry: RootEntry,
    ) -> Result<Job> {
        let env = plan::environment(&spec.env, &user.home);
        let candidates = plan::exec_candidates(&spec.argv[0], plan::path_var(&env));
        let mut keep = Vec::new();
        let [stdin, stdout, stderr] = stdio;
        let stdio = [
            child_stdio(stdin, &mut keep)?,
            child_stdio(stdout, &mut keep)?,
            child_stdio(stderr, &mut keep)?,
        ];
        let mounts = mounts
            .into_iter()
            .map(|op| child_mount(op, &mut keep))
            .collect::<Result<_>>()?;
        let (root_remount, dev_remount) = match spec.isolation {
            // Unused: `Direct` mounts nothing.
            Isolation::Direct => (MountFlags::BIND, [MountFlags::BIND; DEV_NODES.len()]),
            Isolation::Full => (root_remount(&spec.root_dir)?, dev_remount()),
        };
        let envp = env
            .iter()
            .map(|(k, v)| {
                let mut kv = k.as_bytes().to_vec();
                kv.push(b'=');
                kv.extend_from_slice(v.as_bytes());
                cstring(OsStr::from_bytes(&kv))
            })
            .collect::<Result<_>>()?;
        let args = ChildArgs {
            root_dir: cstring(&spec.root_dir)?,
            root_remount,
            mounts,
            dev_remount,
            root_entry,
            hostname: hostname(spec).as_bytes().to_vec(),
            loopback: spec.network == Network::None,
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
            stdio,
            argv: spec.argv.iter().map(cstring).collect::<Result<_>>()?,
            envp,
            candidates: candidates.iter().map(cstring).collect::<Result<_>>()?,
        };
        Ok(Job {
            args,
            flags: libc::c_long::from(clone_flags(spec)),
            isolation: spec.isolation,
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

/// The remount of the root's bind: `nodev`, the root mount's flags kept.
fn root_remount(root_dir: &Path) -> Result<MountFlags> {
    let st = rustix::fs::statvfs(root_dir).map_err(|e| io_err("statvfs", root_dir, e.into()))?;
    Ok(plan::remount_flags(st.f_flag, MountFlags::NODEV))
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

fn child_mount(op: MountOp, keep: &mut Vec<OwnedFd>) -> Result<ChildMount> {
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
            let (fd, _) = open_source(&source, MountKind::File)?;
            let raw = fd.as_raw_fd();
            keep.push(fd);
            ChildMount::File {
                source: raw,
                target: cstring(&target)?,
            }
        }
        MountOp::Bind {
            source,
            target,
            kind,
            read_only,
        } => {
            let (fd, st) = open_source(&source, kind)?;
            let mut add = MountFlags::NODEV;
            if read_only {
                add |= MountFlags::RDONLY;
            }
            let raw = fd.as_raw_fd();
            keep.push(fd);
            ChildMount::Bind {
                source: raw,
                target: cstring(&target)?,
                remount: plan::remount_flags(st.f_flag, add),
            }
        }
    })
}

/// Opens a mount source as an `O_PATH` descriptor the child binds through
/// `/proc/self/fd`, so the bind is of the file checked here: a symlink as
/// the last component is refused, and the type must match `kind`. Returns
/// it with its mount's flags.
fn open_source(p: &Path, kind: MountKind) -> Result<(OwnedFd, rustix::fs::StatVfs)> {
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
    let vfs = rustix::fs::fstatvfs(&fd).map_err(|e| io_err("statvfs", p, e.into()))?;
    Ok((above_stdio(fd)?, vfs))
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

fn cstring(s: impl AsRef<OsStr>) -> Result<CString> {
    let s = s.as_ref();
    CString::new(s.as_bytes())
        .map_err(|_| Error::InvalidSpec(format!("contains a NUL byte: {s:?}")))
}

/// Clones the child, learns whether it reached `execve`, and reaps it.
fn execute(
    job: Job,
    spawn: impl FnOnce(Job) -> Result<Spawned>,
    program: &str,
) -> Result<(ExitStatus, libc::rusage)> {
    let isolation = job.isolation;
    let Spawned { pid, failures } = spawn(job)?;
    // The child is reaped whatever the read or the kill gave.
    let failure = read_failure(&failures);
    drop(failures);
    let killed = if isolation == Isolation::Direct {
        kill_leftovers(pid)
    } else {
        // The child is PID 1 of its namespace: when it exits, the kernel
        // kills the rest.
        Ok(())
    };
    let (status, usage) = reap(pid)?;
    killed?;
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

/// Kills what a `Direct` command left in its process group, as kaniko does,
/// once the command itself has exited. The leader is waited for without
/// being reaped: while it is a zombie, its pid, the group's id, cannot be
/// reused.
fn kill_leftovers(pid: Pid) -> Result<()> {
    loop {
        match rustix::process::waitid(
            WaitId::Pid(pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOWAIT,
        ) {
            Ok(_) => break,
            Err(Errno::INTR) => {}
            Err(e) => return Err(sys_err("waitid", e)),
        }
    }
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
        let sandbox = Sandbox::start(None, dir.path()).unwrap();
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

    #[test]
    fn mount_sources_must_be_what_they_claim() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        fs::create_dir(d.join("cache")).unwrap();
        fs::write(d.join("secret"), b"s").unwrap();
        std::os::unix::fs::symlink("/", d.join("link")).unwrap();
        let (_, vfs) = open_source(&d.join("cache"), MountKind::Directory).unwrap();
        let _ = vfs.f_flag;
        assert!(open_source(&d.join("secret"), MountKind::File).is_ok());
        assert!(open_source(&d.join("secret"), MountKind::Directory).is_err());
        assert!(open_source(&d.join("cache"), MountKind::File).is_err());
        // A symlink as the last component is never followed.
        assert!(open_source(&d.join("link"), MountKind::Directory).is_err());
        let mut keep = Vec::new();
        let m = child_mount(
            MountOp::Bind {
                source: d.join("cache"),
                target: "/root/.cache".into(),
                kind: MountKind::Directory,
                read_only: true,
            },
            &mut keep,
        )
        .unwrap();
        let ChildMount::Bind {
            source, remount, ..
        } = m
        else {
            panic!("{m:?}")
        };
        assert_eq!(keep.len(), 1);
        assert_eq!(source, keep[0].as_raw_fd());
        assert!(remount.contains(MountFlags::BIND | MountFlags::NODEV | MountFlags::RDONLY));
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
