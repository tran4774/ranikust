//! A full-sandbox `RUN`'s cgroups, where ranikust's cgroup v2 tree is
//! delegated to it (ADR-0083 (a), ADR-0110).
//!
//! [`Tree::start`] looks once, when the sandbox starts. ranikust's own
//! cgroup `D` must carry a delegation mark it trusts ([`delegation_mark`]),
//! or be the root of its cgroup namespace on a read-write cgroupfs, and
//! have `cgroup.kill` (Linux 5.14). Where ranikust is alone in `D` and `cpu`
//! is delegated, it nests ([`Layout::Nested`]); elsewhere only the command
//! gets cgroups ([`Layout::Single`]). A probe of every step decides: when
//! it fails, no `RUN` gets a cgroup and the build goes on as where nothing
//! is delegated.
//!
//! Each `RUN` then gets a [`RunCgroup`]. Its command, and its init when
//! nested, move themselves in through `cgroup.procs` descriptors the
//! parent opened (`child.rs`); the command then makes a cgroup namespace
//! rooted at its cgroup. `cgroup.kill` ends what is left on a cancel and
//! once the `RUN` is reaped, and the cgroups are removed before
//! `Sandbox::run` returns, with any cgroup a `RUN` that mounted a cgroupfs
//! of its own made below them. A `SIGKILL`ed ranikust leaves them, empty,
//! until the unit or container ends.

use std::ffi::{CStr, CString, OsStr};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::fs::{AtFlags, FileType, FsWord, Mode, OFlags, ResolveFlags, StatVfsMountFlags};
use rustix::io::Errno;
use rustix::pipe::PipeFlags;
use rustix::process::{Pid, Signal, WaitOptions};
use rustix::thread::UnshareFlags;
use tracing::field::Empty;

use crate::child;
use crate::error::Error;
use crate::fd::fd_above_stdio;

/// Where cgroup v2 must be mounted; a hybrid hierarchy's
/// `/sys/fs/cgroup/unified` is not looked at.
const MOUNT: &str = "/sys/fs/cgroup";

/// `CGROUP2_SUPER_MAGIC`.
const CGROUP2_MAGIC: FsWord = 0x6367_7270;

/// How long a cgroup may take to empty after `cgroup.kill`, and to be
/// removable after that: a `RUN`'s processes are already dead when its
/// init is reaped, so this bounds only a kernel that is slow to say so.
const EMPTY_WITHIN: Duration = Duration::from_secs(10);

/// The most of a cgroup file read: a `cgroup.procs` longer than this does
/// not list ranikust alone anyway.
const READ_LIMIT: usize = 1 << 20;

/// How many `run-<pid>-<n>` names a `RUN` skips that a killed ranikust with
/// the same pid left (ADR-0110, 4) before it fails.
const TAKEN_NAMES: u32 = 64;

/// The step whose `EEXIST` means a `run-` name is taken.
pub(crate) const MAKE_RUN: &str = "create a RUN's cgroup";

const OPEN_RUN: &str = "open a RUN's cgroup";

/// The most cgroups one `RUN`'s removal goes through, its own included: a
/// `RUN` that mounts a cgroupfs of its own may make cgroups below it
/// (ADR-0110). Past it the removal fails and they stay.
pub(crate) const REMOVE_LIMIT: usize = 4096;

/// How many levels below a `RUN`'s cgroup its removal goes; past it, as
/// past [`REMOVE_LIMIT`], the removal fails and they stay.
pub(crate) const REMOVE_DEPTH: usize = 32;

/// The step that finds the cgroups below a `RUN`'s own.
const LIST_BELOW: &str = "list the cgroups below a RUN's cgroup";

/// The limits written on a `RUN`'s cgroup, each with the step that writes
/// it: the kernel then makes no more cgroups below it, its `command` (and
/// `init`, nested) included, than [`REMOVE_LIMIT`] less the `RUN`'s own,
/// and none deeper than [`REMOVE_DEPTH`] (ADR-0110). The command runs in a
/// cgroup namespace of its own rooted at `command`, owned by ranikust's
/// user namespace, where it cannot mount a cgroupfs, and every cgroup
/// namespace it can make is rooted at `command` or below, so it cannot
/// raise these limits, and its removal never fails on those bounds.
const LIMITS: [(&str, usize, &str); 2] = [
    (
        "cgroup.max.descendants",
        REMOVE_LIMIT - 1,
        "write a RUN's cgroup.max.descendants",
    ),
    (
        "cgroup.max.depth",
        REMOVE_DEPTH,
        "write a RUN's cgroup.max.depth",
    ),
];

/// One [`Tree`] per process: nesting moves the whole process, and two
/// sandboxes undoing it would undo each other's.
static CLAIMED: AtomicBool = AtomicBool::new(false);

/// Which cgroups ranikust makes in the delegated one, `D`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Layout {
    /// ranikust moved into `D/ranikust-<pid>` and enabled `+cpu` in `D`;
    /// each `RUN` gets `D/run-*` with `+cpu`, its init in `init` and its
    /// command in `command`, so that the init has a cgroup of its own beside
    /// the command's.
    Nested,
    /// Each `RUN` gets `D/run-*` with no controller and its command in
    /// `command`; the init stays in `D` with ranikust.
    Single,
}

impl Layout {
    /// The `layout` field of `rnk_sandbox.cgroups`.
    fn name(self) -> &'static str {
        match self {
            Layout::Nested => "nested",
            Layout::Single => "single",
        }
    }

    fn describe(self, facts: Facts) -> &'static str {
        match self {
            Layout::Nested => {
                "nested with +cpu: each RUN's init and command in cgroups of their own"
            }
            Layout::Single if !facts.alone => {
                "each RUN's command in a cgroup of its own, without a controller (ranikust \
                 is not alone in its cgroup)"
            }
            Layout::Single => {
                "each RUN's command in a cgroup of its own, without a controller (cpu is \
                 not delegated)"
            }
        }
    }
}

/// What [`decide`] needs to know of `D`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Facts {
    /// A delegation mark ranikust trusts ([`trusted_marks`]) is `1` on `D`
    /// itself; not read, so `false`, where `D` is the root of ranikust's
    /// cgroup namespace (`ns_root`).
    marked: bool,
    /// `D` is the root of ranikust's cgroup namespace (`0::/`).
    ns_root: bool,
    /// The cgroupfs is mounted read-only.
    read_only: bool,
    /// `D` has `cgroup.kill`: Linux 5.14, and not the root cgroup.
    kill: bool,
    /// `D`'s `cgroup.type` is `domain`.
    domain: bool,
    /// `D`'s `cgroup.procs` lists ranikust alone.
    alone: bool,
    /// `cpu` is in `D`'s `cgroup.controllers`.
    cpu: bool,
}

/// The layout for `D`, or why `D` gets none (ADR-0110).
fn decide(f: Facts) -> Result<Layout, &'static str> {
    if !f.marked && !f.ns_root {
        return Err(
            "its cgroup carries no delegation mark and is not the root of its cgroup \
                    namespace",
        );
    }
    if f.read_only {
        return Err("the cgroupfs is mounted read-only");
    }
    // The root cgroup has no `cgroup.kill` either, so a namespace root that
    // has one is not the host's.
    if !f.kill {
        return Err("its cgroup has no cgroup.kill (Linux 5.14)");
    }
    if !f.domain {
        return Err("its cgroup is not a domain cgroup");
    }
    if f.alone && f.cpu {
        Ok(Layout::Nested)
    } else {
        Ok(Layout::Single)
    }
}

/// A cgroup step that failed, and its errno.
#[derive(Debug)]
pub(crate) struct Failed {
    step: &'static str,
    source: io::Error,
}

/// Whether `f` is a `run-` name a killed ranikust left: only the first
/// `mkdir`'s `EEXIST`; one below it means another writer in `D`.
fn name_taken(f: &Failed) -> bool {
    f.step == MAKE_RUN && f.source.raw_os_error() == Some(libc::EEXIST)
}

fn step<T>(step: &'static str, r: rustix::io::Result<T>) -> Result<T, Failed> {
    r.map_err(|e| Failed {
        step,
        source: e.into(),
    })
}

/// The cgroup path `/proc/self/cgroup` gives for cgroup v2 (`0::/...`),
/// without its leading `/`: empty for the root of ranikust's cgroup
/// namespace. `None` without one, or when it climbs out of that namespace
/// (`/..`), which no `openat` below the mount may follow.
fn own_cgroup(proc_self_cgroup: &[u8]) -> Option<&[u8]> {
    let path = proc_self_cgroup
        .split(|&b| b == b'\n')
        .find_map(|l| l.strip_prefix(b"0::"))?
        .strip_prefix(b"/")?;
    if path.is_empty() {
        return Some(path);
    }
    path.split(|&b| b == b'/')
        .all(|c| !c.is_empty() && c != b"." && c != b"..")
        .then_some(path)
}

/// Whether `cgroup.procs` lists `me`, and whether it lists nothing else.
fn membership(procs: &[u8], me: i32) -> (bool, bool) {
    let mut listed = false;
    let mut others = false;
    for line in procs.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
        // A pid outside ranikust's PID namespace reads as 0, and a line
        // that does not parse is no proof of ranikust: both count as
        // another process.
        let pid = std::str::from_utf8(line)
            .ok()
            .and_then(|s| s.parse::<i32>().ok());
        if pid == Some(me) {
            listed = true;
        } else {
            others = true;
        }
    }
    (listed, listed && !others)
}

/// Whether a `cgroup.controllers` list names `cpu`.
fn has_cpu(controllers: &[u8]) -> bool {
    controllers
        .split(u8::is_ascii_whitespace)
        .any(|c| c == b"cpu")
}

/// `populated` of a `cgroup.events`; `None` without that line.
fn populated(events: &[u8]) -> Option<bool> {
    events
        .split(|&b| b == b'\n')
        .find_map(|l| l.strip_prefix(b"populated "))
        .and_then(|v| match v {
            b"0" => Some(false),
            b"1" => Some(true),
            _ => None,
        })
}

fn open_in<P: rustix::path::Arg>(
    dir: BorrowedFd<'_>,
    name: P,
    flags: OFlags,
) -> rustix::io::Result<OwnedFd> {
    rustix::fs::openat(
        dir,
        name,
        flags | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
}

/// One `write` of all of `data`: a cgroup file takes a value whole.
fn write_all(fd: BorrowedFd<'_>, data: &[u8]) -> rustix::io::Result<()> {
    loop {
        match rustix::io::write(fd, data) {
            Ok(n) if n == data.len() => return Ok(()),
            Ok(_) => return Err(Errno::IO),
            Err(Errno::INTR) => {}
            Err(e) => return Err(e),
        }
    }
}

fn write_in(dir: BorrowedFd<'_>, name: &str, data: &[u8]) -> rustix::io::Result<()> {
    let fd = open_in(dir, name, OFlags::WRONLY)?;
    write_all(fd.as_fd(), data)
}

fn read_in(dir: BorrowedFd<'_>, name: &str) -> rustix::io::Result<Vec<u8>> {
    let fd = open_in(dir, name, OFlags::RDONLY)?;
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match rustix::io::read(&fd, &mut buf) {
            Ok(0) => return Ok(out),
            Ok(n) => {
                out.extend_from_slice(buf.get(..n).unwrap_or_default());
                if out.len() > READ_LIMIT {
                    return Err(Errno::FBIG);
                }
            }
            Err(Errno::INTR) => {}
            Err(e) => return Err(e),
        }
    }
}

fn mkdir_in(dir: BorrowedFd<'_>, name: &str) -> rustix::io::Result<()> {
    rustix::fs::mkdirat(dir, name, Mode::from_raw_mode(0o755))
}

/// Removes the cgroup `name`, again while the kernel still counts a
/// process that has just exited (`EBUSY`), until `deadline`.
fn rmdir_in<P: rustix::path::Arg + Copy>(
    dir: BorrowedFd<'_>,
    name: P,
    deadline: Instant,
) -> rustix::io::Result<()> {
    loop {
        match rustix::fs::unlinkat(dir, name, AtFlags::REMOVEDIR) {
            Err(Errno::BUSY) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(Errno::INTR) => {}
            r => return r,
        }
    }
}

/// A cgroup in `dir`, opened to list it, never through a link or onto
/// another mount: what is walked below a `RUN`'s cgroup stays cgroups of
/// this cgroupfs, whatever names the `RUN` made there.
fn open_below<P: rustix::path::Arg + Copy>(
    dir: BorrowedFd<'_>,
    name: P,
) -> rustix::io::Result<OwnedFd> {
    let mut attempts = 0;
    loop {
        match rustix::fs::openat2(
            dir,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
            ResolveFlags::BENEATH
                | ResolveFlags::NO_XDEV
                | ResolveFlags::NO_SYMLINKS
                | ResolveFlags::NO_MAGICLINKS,
        ) {
            // A rename raced the lookup; the kernel asks to retry.
            Err(Errno::AGAIN) if attempts < 16 => attempts += 1,
            r => return r,
        }
    }
}

/// Writes [`LIMITS`] to the cgroup `dir`.
fn limit(dir: BorrowedFd<'_>) -> Result<(), Failed> {
    for (file, n, what) in LIMITS {
        step(what, write_in(dir, file, n.to_string().as_bytes()))?;
    }
    Ok(())
}

/// Removes the cgroup `name` in `dir` and every cgroup below it, the
/// deepest first, each opened relative to its parent's descriptor with
/// [`open_below`]; `left` counts down what [`REMOVE_LIMIT`] still allows. A
/// child that cannot be removed, such as one a process still holds, fails
/// it at once: its parent never could be. On a failure `at` is the path,
/// below `name`, of the cgroup that failed; empty for `name` itself.
fn remove_tree<P: rustix::path::Arg + Copy>(
    dir: BorrowedFd<'_>,
    name: P,
    depth: usize,
    left: &mut usize,
    deadline: Instant,
    at: &mut PathBuf,
) -> Result<(), Failed> {
    let too_many = |what: String| Failed {
        step: LIST_BELOW,
        source: io::Error::other(what),
    };
    *left = left
        .checked_sub(1)
        .ok_or_else(|| too_many(format!("more than {REMOVE_LIMIT} cgroups")))?;
    let sub = step(LIST_BELOW, open_below(dir, name))?;
    let mut below: Vec<CString> = Vec::new();
    for entry in step(LIST_BELOW, rustix::fs::Dir::read_from(&sub))? {
        let entry = step(LIST_BELOW, entry)?;
        let n = entry.file_name();
        if entry.file_type() != FileType::Directory || n == c"." || n == c".." {
            continue;
        }
        if below.len() >= *left {
            return Err(too_many(format!("more than {REMOVE_LIMIT} cgroups")));
        }
        below.push(n.to_owned());
    }
    if !below.is_empty() && depth >= REMOVE_DEPTH {
        return Err(too_many(format!("cgroups more than {REMOVE_DEPTH} deep")));
    }
    for child in &below {
        if let Err(f) = remove_tree::<&CStr>(sub.as_fd(), child, depth + 1, left, deadline, at) {
            let mut path = PathBuf::from(OsStr::from_bytes(child.to_bytes()));
            if !at.as_os_str().is_empty() {
                path.push(&*at);
            }
            *at = path;
            return Err(f);
        }
    }
    drop(sub);
    step("remove a RUN's cgroup", rmdir_in(dir, name, deadline))
}

/// Waits until the cgroup whose `cgroup.events` is `events` holds no
/// process, below it included, or `deadline` passes (`ETIMEDOUT`).
fn wait_empty(events: BorrowedFd<'_>, deadline: Instant) -> rustix::io::Result<()> {
    loop {
        let mut buf = [0u8; 256];
        let n = loop {
            match rustix::io::pread(events, &mut buf, 0) {
                Ok(n) => break n,
                Err(Errno::INTR) => {}
                Err(e) => return Err(e),
            }
        };
        match populated(buf.get(..n).unwrap_or_default()) {
            Some(false) => return Ok(()),
            Some(true) => {}
            None => return Err(Errno::PROTO),
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(Errno::TIMEDOUT);
        }
        let timeout = Timespec {
            tv_sec: i64::try_from(left.as_secs()).unwrap_or(i64::MAX),
            tv_nsec: i64::from(left.subsec_nanos()),
        };
        // The kernel flags `cgroup.events` with `POLLPRI` when it changes
        // after the read above.
        let mut fds = [PollFd::new(&events, PollFlags::PRI)];
        match rustix::event::poll(&mut fds, Some(&timeout)) {
            Ok(_) | Err(Errno::INTR) => {}
            Err(e) => return Err(e),
        }
    }
}

/// Whether `fd` is on a cgroup v2 filesystem.
fn is_cgroup2(fd: BorrowedFd<'_>) -> rustix::io::Result<bool> {
    rustix::fs::fstatfs(fd).map(|st| st.f_type == CGROUP2_MAGIC)
}

/// `None` for a file the cgroup does not have; any other error stays one.
fn if_present(r: rustix::io::Result<Vec<u8>>) -> rustix::io::Result<Option<Vec<u8>>> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(Errno::NOENT) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Whether `dir` carries a delegation mark, `1`, that a ranikust with
/// effective uid `euid` trusts ([`trusted_marks`]).
fn delegation_mark(dir: BorrowedFd<'_>, euid: u32) -> rustix::io::Result<bool> {
    // Root trusts `trusted.delegate` alone: no owner to compare.
    let owner = if euid == 0 {
        0
    } else {
        rustix::fs::fstat(dir)?.st_uid
    };
    for &name in trusted_marks(euid, owner) {
        let mut buf = [0u8; 8];
        let value = rustix::fs::fgetxattr(dir, name, &mut buf[..]);
        if is_mark(value.map(|n| buf.get(..n).unwrap_or_default()))? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The marks a ranikust with effective uid `euid` trusts on a cgroup that
/// `owner` owns (ADR-0110, 1): `trusted.delegate`, which only
/// `CAP_SYS_ADMIN` in the initial user namespace can set; `user.delegate`
/// only for a ranikust that is not root and owns the cgroup, since anyone
/// who may write the cgroup's inode can set it, a root `RUN` with a cgroupfs
/// of its own included. `euid` is ranikust's in its own user namespace:
/// rootless overlay mode, which starts the sandbox in the build's user
/// namespace, counts as root.
fn trusted_marks(euid: u32, owner: u32) -> &'static [&'static str] {
    if euid != 0 && owner == euid {
        &["trusted.delegate", "user.delegate"]
    } else {
        &["trusted.delegate"]
    }
}

/// Whether an xattr read as `value` is the mark: `1`. Absent, unsupported
/// or longer than the buffer is no mark; any other error stays one.
fn is_mark(value: rustix::io::Result<&[u8]>) -> rustix::io::Result<bool> {
    match value {
        Ok(v) => Ok(v == b"1"),
        Err(Errno::NODATA | Errno::NOTSUP | Errno::RANGE) => Ok(false),
        Err(e) => Err(e),
    }
}

/// ranikust's cgroup, open, its path for messages, and its [`Facts`].
struct Found {
    dir: OwnedFd,
    path: String,
    facts: Facts,
}

/// Why [`detect`] found no cgroup to probe: [`Unused`] without the probe.
#[derive(Debug)]
enum NotFound {
    /// As [`Unused::NotDelegated`].
    NotDelegated(String),
    /// As [`Unused::Unreadable`].
    Unreadable(String),
}

impl From<NotFound> for Unused {
    fn from(n: NotFound) -> Self {
        match n {
            NotFound::NotDelegated(why) => Unused::NotDelegated(why),
            NotFound::Unreadable(why) => Unused::Unreadable(why),
        }
    }
}

/// Opens ranikust's own cgroup and reads what [`decide`] needs; only reads.
/// A cgroup with neither a mark nor a namespace's root, or on a read-only
/// cgroupfs, is read no further.
fn detect() -> Result<Found, NotFound> {
    let not = NotFound::NotDelegated;
    let own = std::fs::read("/proc/self/cgroup")
        .map_err(|e| not(format!("/proc/self/cgroup cannot be read: {e}")))?;
    let rel = own_cgroup(&own).ok_or_else(|| {
        not("/proc/self/cgroup names no cgroup v2 path below its namespace's root".into())
    })?;
    let path = if rel.is_empty() {
        MOUNT.to_owned()
    } else {
        format!("{MOUNT}/{}", String::from_utf8_lossy(rel))
    };
    let base = rustix::fs::open(
        MOUNT,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|e| not(format!("{MOUNT} cannot be opened: {e}")))?;
    if !is_cgroup2(base.as_fd()).map_err(|e| not(format!("{MOUNT} cannot be stat'ed: {e}")))? {
        return Err(not(format!("no cgroup v2 is mounted at {MOUNT}")));
    }
    let dir = if rel.is_empty() {
        base
    } else {
        rustix::fs::openat(
            &base,
            rel,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|e| not(format!("{path} cannot be opened: {e}")))?
    };
    if !is_cgroup2(dir.as_fd()).map_err(|e| not(format!("{path} cannot be stat'ed: {e}")))? {
        return Err(not(format!("{path} is not on cgroup v2")));
    }
    let ns_root = rel.is_empty();
    // A namespace's root counts without a mark, so its read cannot fail it.
    let euid = rustix::process::geteuid().as_raw();
    let marked = !ns_root
        && delegation_mark(dir.as_fd(), euid).map_err(|e| {
            NotFound::Unreadable(format!("{path}'s delegation mark cannot be read: {e}"))
        })?;
    if !marked && !ns_root {
        // `decide` says why.
        return Ok(Found {
            dir,
            path,
            facts: Facts::default(),
        });
    }
    let read_only = rustix::fs::fstatvfs(&dir)
        .map_err(|e| NotFound::Unreadable(format!("{path} cannot be stat'ed: {e}")))?
        .f_flag
        .contains(StatVfsMountFlags::RDONLY);
    if read_only {
        // As above: not delegated, read no further.
        return Ok(Found {
            dir,
            path,
            facts: Facts {
                marked,
                ns_root,
                read_only,
                ..Facts::default()
            },
        });
    }
    // From here `D` looks delegated: a file it cannot read is worth a warning.
    let unreadable = |what: &str, e: Errno| NotFound::Unreadable(format!("{path}{what}: {e}"));
    let me = rustix::process::getpid().as_raw_nonzero().get();
    let procs = read_in(dir.as_fd(), "cgroup.procs")
        .map_err(|e| unreadable("/cgroup.procs cannot be read", e))?;
    let (listed, alone) = membership(&procs, me);
    // By identity, not by path: the mount could show another tree there.
    if !listed {
        return Err(not(format!(
            "{path} does not list ranikust: the cgroupfs at {MOUNT} is not its own"
        )));
    }
    let kill = match rustix::fs::statat(&dir, "cgroup.kill", AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => true,
        Err(Errno::NOENT) => false,
        Err(e) => return Err(unreadable("/cgroup.kill cannot be looked up", e)),
    };
    // The root cgroup has no `cgroup.type`.
    let domain = if_present(read_in(dir.as_fd(), "cgroup.type"))
        .map_err(|e| unreadable("/cgroup.type cannot be read", e))?
        .is_some_and(|t| t.trim_ascii() == b"domain");
    let cpu = if_present(read_in(dir.as_fd(), "cgroup.controllers"))
        .map_err(|e| unreadable("/cgroup.controllers cannot be read", e))?
        .is_some_and(|c| has_cpu(&c));
    let facts = Facts {
        marked,
        ns_root,
        read_only,
        kill,
        domain,
        alone,
        cpu,
    };
    Ok(Found { dir, path, facts })
}

/// The delegated cgroup `D` ranikust found and probed, and what it changed
/// there, which dropping it undoes.
#[derive(Debug)]
pub(crate) struct Tree {
    dir: OwnedFd,
    path: String,
    layout: Layout,
    /// What the layout was chosen from, for the log.
    facts: Facts,
    pid: i32,
    next: AtomicU64,
    /// Nested: ranikust's own leaf, once made.
    leaf: Option<String>,
    /// Nested: ranikust moved into its leaf.
    moved: bool,
    /// Nested: `+cpu` is on in `D`'s `cgroup.subtree_control`.
    cpu: bool,
}

impl Tree {
    /// Finds the delegated tree and probes it, or says at `debug` why there
    /// is none, at `warn` that the delegation mark or a file of a marked or
    /// namespace-root cgroup could not be read, or which step of the probe
    /// failed; logs the layout at `info`. One `rnk_sandbox.cgroups` span.
    pub(crate) fn start() -> Option<Tree> {
        let span = tracing::debug_span!("rnk_sandbox.cgroups", layout = Empty);
        let _entered = span.enter();
        match Self::try_start() {
            Ok(tree) => {
                span.record("layout", tree.layout.name());
                tracing::info!(
                    "rnk-sandbox: RUN cgroups in {}: {}",
                    tree.path,
                    tree.layout.describe(tree.facts)
                );
                Some(tree)
            }
            Err(Unused::NotDelegated(why)) => {
                span.record("layout", "none");
                tracing::debug!("rnk-sandbox: no RUN gets a cgroup of its own: {why}");
                None
            }
            Err(Unused::Unreadable(why)) => {
                span.record("layout", "none");
                tracing::warn!("rnk-sandbox: no RUN gets a cgroup of its own: {why} (ADR-0110)");
                None
            }
            Err(Unused::ProbeFailed { path, failed }) => {
                span.record("layout", "none");
                tracing::warn!(
                    "rnk-sandbox: no RUN gets a cgroup of its own: in {path}, the start-up \
                     probe could not {}: {} (ADR-0110)",
                    failed.step,
                    failed.source
                );
                None
            }
        }
    }

    pub(crate) fn try_start() -> Result<Tree, Unused> {
        let found = detect()?;
        let layout = decide(found.facts)
            .map_err(|why| Unused::NotDelegated(format!("{}: {why}", found.path)))?;
        if CLAIMED.swap(true, Ordering::AcqRel) {
            return Err(Unused::NotDelegated(
                "another sandbox of this process holds them".into(),
            ));
        }
        let mut tree = Tree {
            dir: found.dir,
            path: found.path,
            layout,
            facts: found.facts,
            pid: rustix::process::getpid().as_raw_nonzero().get(),
            next: AtomicU64::new(0),
            leaf: None,
            moved: false,
            cpu: false,
        };
        match tree.probe() {
            Ok(()) => Ok(tree),
            // Dropping the tree undoes what the probe changed.
            Err(failed) => Err(Unused::ProbeFailed {
                path: tree.path.clone(),
                failed,
            }),
        }
    }

    /// Every step a `RUN` takes, once: nesting where the layout says so,
    /// then a `RUN`'s cgroups with a child moved into each, which makes a
    /// cgroup namespace in `command`, `cgroup.kill`, and their removal.
    fn probe(&mut self) -> Result<(), Failed> {
        if self.layout == Layout::Nested {
            self.nest()?;
        }
        let run = self.make(&format!("probe-{}", self.pid))?;
        let moved = probe_child(&run);
        let removed = run.remove_steps().map_err(|(f, _)| f);
        moved.and(removed)
    }

    /// Moves ranikust into a leaf of its own, then enables `+cpu` in `D`,
    /// which no cgroup holding a process may do.
    fn nest(&mut self) -> Result<(), Failed> {
        // `rnk-sched`'s memory gate and `rnk-metrics` read this name's parent.
        let leaf = format!("ranikust-{}", self.pid);
        step(
            "create ranikust's own cgroup",
            mkdir_in(self.dir.as_fd(), &leaf),
        )?;
        let procs = step(
            "open ranikust's own cgroup",
            open_in(
                self.dir.as_fd(),
                format!("{leaf}/cgroup.procs"),
                OFlags::WRONLY,
            ),
        );
        self.leaf = Some(leaf);
        // Writing 0 moves the writer's whole process, every thread.
        step(
            "move ranikust into its own cgroup",
            write_all(procs?.as_fd(), b"0"),
        )?;
        self.moved = true;
        // `EBUSY` if a process joined `D` after `detect` read it: a failed
        // probe (ADR-0110, 3).
        step(
            "enable the cpu controller",
            write_in(self.dir.as_fd(), "cgroup.subtree_control", b"+cpu"),
        )?;
        self.cpu = true;
        Ok(())
    }

    /// The cgroups of the next `RUN`, made before its clone.
    ///
    /// # Errors
    ///
    /// [`Error::Cgroup`] naming the step, the cgroup and the errno; nothing
    /// is left behind.
    pub(crate) fn run_cgroup(&self) -> Result<RunCgroup<'_>, Error> {
        let mut skipped = 0;
        loop {
            let n = self.next.fetch_add(1, Ordering::Relaxed);
            let name = format!("run-{}-{n}", self.pid);
            match self.make(&name) {
                Ok(run) => return Ok(run),
                Err(f) if name_taken(&f) && skipped < TAKEN_NAMES => {
                    skipped += 1;
                }
                Err(f) => {
                    return Err(Error::Cgroup {
                        op: f.step,
                        path: format!("{}/{name}", self.path).into(),
                        source: f.source,
                    });
                }
            }
        }
    }

    fn make(&self, name: &str) -> Result<RunCgroup<'_>, Failed> {
        step(MAKE_RUN, mkdir_in(self.dir.as_fd(), name))?;
        let made = self.fill(name);
        if made.is_err() {
            for sub in [
                format!("{name}/command"),
                format!("{name}/init"),
                name.to_owned(),
            ] {
                // Empty, as no process joined yet; a missing one was never
                // made. The step's own failure is what the caller reports.
                match rustix::fs::unlinkat(&self.dir, sub.as_str(), AtFlags::REMOVEDIR) {
                    Ok(()) | Err(Errno::NOENT) => {}
                    Err(e) => {
                        tracing::warn!("rnk-sandbox: cannot remove {}/{sub}: {e}", self.path);
                    }
                }
            }
        }
        made
    }

    fn fill(&self, name: &str) -> Result<RunCgroup<'_>, Failed> {
        // Above 0–2: the child's `dup2` onto them must not close one.
        let procs = |dir: BorrowedFd<'_>, file: &str| {
            step(
                OPEN_RUN,
                open_in(dir, file, OFlags::WRONLY).and_then(fd_above_stdio),
            )
        };
        let dir = step(
            OPEN_RUN,
            open_in(self.dir.as_fd(), name, OFlags::RDONLY | OFlags::DIRECTORY),
        )?;
        // Before a process joins it or a cgroup is made below it.
        limit(dir.as_fd())?;
        let kill = step(
            OPEN_RUN,
            open_in(dir.as_fd(), "cgroup.kill", OFlags::WRONLY),
        )?;
        let events = step(
            OPEN_RUN,
            open_in(dir.as_fd(), "cgroup.events", OFlags::RDONLY),
        )?;
        let init = match self.layout {
            Layout::Nested => {
                step(
                    "enable the cpu controller in a RUN's cgroup",
                    write_in(dir.as_fd(), "cgroup.subtree_control", b"+cpu"),
                )?;
                step("create a RUN's init cgroup", mkdir_in(dir.as_fd(), "init"))?;
                Some(procs(dir.as_fd(), "init/cgroup.procs")?)
            }
            Layout::Single => None,
        };
        // The limits sit above every cgroup namespace the `RUN` can make, all
        // rooted at `command` or below, so no cgroupfs it can mount shows
        // them, `nsdelegate` or not.
        step(
            "create a RUN's command cgroup",
            mkdir_in(dir.as_fd(), "command"),
        )?;
        let command = procs(dir.as_fd(), "command/cgroup.procs")?;
        Ok(RunCgroup {
            tree: self,
            name: name.to_owned(),
            dir,
            kill: Killer(Arc::new(kill)),
            events,
            init,
            command,
        })
    }

    /// The layout the probe chose.
    #[cfg(test)]
    pub(crate) fn layout(&self) -> Layout {
        self.layout
    }

    /// `D`'s path, for messages.
    #[cfg(test)]
    pub(crate) fn path(&self) -> &str {
        &self.path
    }

    /// Undoes the nesting: `-cpu` first, as no process may join `D` while
    /// it is on, then ranikust back into `D`, then its leaf removed. A step
    /// that fails leaves the rest, as a killed ranikust would.
    fn unnest(&mut self) -> Result<(), Failed> {
        if self.cpu {
            step(
                "disable the cpu controller",
                write_in(self.dir.as_fd(), "cgroup.subtree_control", b"-cpu"),
            )?;
            self.cpu = false;
        }
        if self.moved {
            step(
                "move ranikust back into its cgroup",
                write_in(self.dir.as_fd(), "cgroup.procs", b"0"),
            )?;
            self.moved = false;
        }
        if let Some(leaf) = &self.leaf {
            // A process that joined the leaf (a `docker exec`) keeps it busy
            // however long ranikust waits; an unreadable list waits no
            // longer either, and the `rmdir` reports.
            let joined = read_in(self.dir.as_fd(), &format!("{leaf}/cgroup.procs"))
                .map_or(true, |p| !p.trim_ascii().is_empty());
            let wait = if joined { Duration::ZERO } else { EMPTY_WITHIN };
            step(
                "remove ranikust's own cgroup",
                rmdir_in(self.dir.as_fd(), leaf, Instant::now() + wait),
            )?;
            self.leaf = None;
        }
        Ok(())
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        if let Err(f) = self.unnest() {
            tracing::warn!(
                "rnk-sandbox: in {}, could not {}: {}; its cgroups stay until the unit or \
                 container ends",
                self.path,
                f.step,
                f.source
            );
        }
        CLAIMED.store(false, Ordering::Release);
    }
}

/// Why [`Tree::try_start`] gives no tree.
#[derive(Debug)]
pub(crate) enum Unused {
    /// No delegated tree here, or another sandbox holds it.
    NotDelegated(String),
    /// A delegation mark that cannot be read, or a file of a cgroup that
    /// carries the mark or is a namespace's root.
    Unreadable(String),
    /// A delegated tree whose probe failed at a step.
    ProbeFailed { path: String, failed: Failed },
}

/// Writes a `RUN`'s `cgroup.kill` from any thread (a cancel).
#[derive(Clone, Debug)]
pub(crate) struct Killer(Arc<OwnedFd>);

impl Killer {
    pub(crate) fn kill(&self) -> rustix::io::Result<()> {
        write_all(self.0.as_fd(), b"1")
    }
}

/// The cgroups of one `RUN`: `D/run-*` with its `command` leaf, and its
/// `init` leaf when nested. Removed by [`RunCgroup::remove`], never on drop.
#[derive(Debug)]
pub(crate) struct RunCgroup<'t> {
    tree: &'t Tree,
    name: String,
    dir: OwnedFd,
    kill: Killer,
    events: OwnedFd,
    /// Nested: `init/cgroup.procs`.
    init: Option<OwnedFd>,
    /// `command/cgroup.procs`.
    command: OwnedFd,
}

impl RunCgroup<'_> {
    /// The descriptor the init writes `0` to, numbered as in the child.
    pub(crate) fn init_procs(&self) -> Option<RawFd> {
        self.init.as_ref().map(AsRawFd::as_raw_fd)
    }

    /// The descriptor the command writes `0` to.
    pub(crate) fn command_procs(&self) -> RawFd {
        self.command.as_raw_fd()
    }

    pub(crate) fn killer(&self) -> Killer {
        self.kill.clone()
    }

    /// Kills what is left in the cgroups, waits until they are empty and
    /// removes them, before the caller diffs the root.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] naming the step, the cgroup (the one below the `RUN`'s
    /// that failed, if one did) and the errno: the `RUN` fails (ADR-0110).
    pub(crate) fn remove(self) -> Result<(), Error> {
        let run = PathBuf::from(format!("{}/{}", self.tree.path, self.name));
        self.remove_steps().map_err(|(f, at)| Error::Io {
            op: f.step,
            path: if at.as_os_str().is_empty() {
                run
            } else {
                run.join(at)
            },
            source: f.source,
        })
    }

    /// Every step of [`RunCgroup::remove`], each tried; the first failure,
    /// with the path below the `RUN`'s cgroup of the one that failed.
    fn remove_steps(self) -> Result<(), (Failed, PathBuf)> {
        let deadline = Instant::now() + EMPTY_WITHIN;
        let emptied = step("write a RUN's cgroup.kill", self.kill.kill()).and_then(|()| {
            step(
                "wait for a RUN's cgroup to empty",
                wait_empty(self.events.as_fd(), deadline),
            )
        });
        let RunCgroup {
            tree,
            name,
            dir,
            init,
            command,
            ..
        } = self;
        drop((dir, init, command));
        let mut left = REMOVE_LIMIT;
        let mut at = PathBuf::new();
        let removed = remove_tree(
            tree.dir.as_fd(),
            name.as_str(),
            0,
            &mut left,
            deadline,
            &mut at,
        );
        match (emptied, removed) {
            (Err(f), _) => Err((f, PathBuf::new())),
            (Ok(()), r) => r.map_err(|f| (f, at)),
        }
    }
}

/// Moves a child into the `RUN` cgroups of the probe as a `RUN`'s init and
/// command move themselves, and has it make a cgroup namespace there as
/// the command does; then kills it with `cgroup.kill`, waits until the
/// cgroup is empty and reaps it, checking it died of `SIGKILL`.
fn probe_child(run: &RunCgroup<'_>) -> Result<(), Failed> {
    let procs = [run.init_procs().unwrap_or(-1), run.command_procs()];
    let (report, report_w) = step(
        "make the probe's pipe",
        rustix::pipe::pipe_with(PipeFlags::CLOEXEC),
    )?;
    let parent = rustix::process::getpid();
    let null: libc::c_long = 0;
    // SAFETY: as the spawner's clone: no `CLONE_VM`, a null stack, so the
    // child continues here on a copy of this thread's stack and memory. The
    // arguments are integers.
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
        // SAFETY: the child of that clone, right after it returned 0.
        unsafe { probe_child_main(procs, report_w.as_raw_fd(), parent) }
    }
    drop(report_w);
    if ret < 0 {
        return Err(Failed {
            step: "fork the probe's child",
            source: io::Error::last_os_error(),
        });
    }
    let pid = i32::try_from(ret)
        .ok()
        .and_then(Pid::from_raw)
        .ok_or(Failed {
            step: "fork the probe's child",
            source: io::Error::from(io::ErrorKind::InvalidData),
        })?;
    let emptied = read_report(&report)
        .and_then(|()| step("write a RUN's cgroup.kill", run.kill.kill()))
        .and_then(|()| {
            step(
                "wait for a RUN's cgroup to empty",
                wait_empty(run.events.as_fd(), Instant::now() + EMPTY_WITHIN),
            )
        });
    if emptied.is_err() {
        // Not killed through its cgroup: killed here, so the reap returns;
        // `ESRCH` when it already exited.
        let _ = rustix::process::kill_process(pid, Signal::KILL);
    }
    let reaped = loop {
        match rustix::process::waitpid(Some(pid), WaitOptions::empty()) {
            Err(Errno::INTR) => {}
            r => break r,
        }
    };
    emptied?;
    match reaped {
        Ok(Some((_, st))) if st.terminating_signal() == Some(libc::SIGKILL) => Ok(()),
        Ok(_) => Err(Failed {
            step: "write a RUN's cgroup.kill",
            source: io::Error::other("the probe's child did not die of SIGKILL"),
        }),
        Err(e) => Err(Failed {
            step: "reap the probe's child",
            source: e.into(),
        }),
    }
}

/// What the probe's report names in an `F` record after `0` (the init's
/// cgroup) and `1` (the command's): the cgroup namespace it makes.
const NAMESPACE: u8 = 2;

/// `R` once the probe's child joined every cgroup and made its cgroup
/// namespace; `F`, which step, and the errno when it could not.
fn read_report(fd: &OwnedFd) -> Result<(), Failed> {
    let mut record = [0u8; 6];
    let mut len = 0;
    while let Some(rest) = record.get_mut(len..).filter(|r| !r.is_empty()) {
        match rustix::io::read(fd, rest) {
            Ok(0) => break,
            Ok(n) => len += n,
            Err(Errno::INTR) => {}
            Err(e) => return step("read the probe's report", Err(e)),
        }
        if record[0] == b'R' {
            return Ok(());
        }
    }
    match (len, record) {
        (6, [b'F', which, a, b, c, d]) => Err(Failed {
            step: match which {
                0 => "move a child into a RUN's init cgroup",
                1 => "move a child into a RUN's command cgroup",
                NAMESPACE => "make a cgroup namespace in a RUN's command cgroup",
                _ => "finish a step it did not name",
            },
            source: io::Error::from_raw_os_error(i32::from_le_bytes([a, b, c, d])),
        }),
        // Dead before it reported, at whichever step: name none.
        _ => Err(Failed {
            step: "get a report from its child",
            source: io::Error::from(io::ErrorKind::UnexpectedEof),
        }),
    }
}

/// The probe's child: writes `0` to each `cgroup.procs` of `procs` in turn
/// (`-1` is none), as a `RUN`'s init and command do, then makes a cgroup
/// namespace as the command does, reports on `report`, and waits for
/// `cgroup.kill`. Only system calls, as `child.rs`.
///
/// # Safety
///
/// Call only in the child of a `clone` without `CLONE_VM`, right after it
/// returned 0.
unsafe fn probe_child_main(procs: [RawFd; 2], report: RawFd, parent: Pid) -> ! {
    child::default_signals();
    // It dies with the probing thread, whatever the probe does next.
    let armed = rustix::process::set_parent_process_death_signal(Some(Signal::KILL)).is_ok();
    if !armed || rustix::process::getppid() != Some(parent) {
        // SAFETY: `_exit` ends this process at once.
        unsafe { libc::_exit(1) }
    }
    // SAFETY: the pipe's write end, open in this process.
    let report = unsafe { BorrowedFd::borrow_raw(report) };
    let failed = |which: u8, e: Errno| -> ! {
        let [a, b, c, d] = e.raw_os_error().to_le_bytes();
        // Nothing to do if the parent is gone: it reads end-of-file.
        let _ = rustix::io::write(report, &[b'F', which, a, b, c, d]);
        // SAFETY: `_exit` ends this process at once.
        unsafe { libc::_exit(1) }
    };
    for (which, fd) in (0u8..).zip(procs) {
        if fd < 0 {
            continue;
        }
        // SAFETY: open in this process, copied from the parent's.
        if let Err(e) = write_all(unsafe { BorrowedFd::borrow_raw(fd) }, b"0") {
            failed(which, e);
        }
    }
    // In `command` now, as the command makes its cgroup namespace there.
    // SAFETY: `NEWCGROUP` does not unshare the descriptor table, the only
    // flag `unshare_unsafe` is unsafe for; this child is single-threaded.
    if let Err(e) = unsafe { rustix::thread::unshare_unsafe(UnshareFlags::NEWCGROUP) } {
        failed(NAMESPACE, e);
    }
    // As above: a parent gone reads nothing.
    let _ = rustix::io::write(report, b"R");
    loop {
        // SAFETY: a plain syscall; the child waits for `SIGKILL`.
        unsafe { libc::pause() };
    }
}

#[cfg(test)]
mod tests;
