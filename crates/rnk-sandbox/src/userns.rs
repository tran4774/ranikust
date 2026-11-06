//! The user-namespace probe: step 2 of
//! `design/modes-and-detection.md#probe-order---modeauto`, reported only:
//! nothing routes on it yet.
//!
//! Each attempt runs in a forked child that makes a user namespace, gets an
//! id map, makes a mount namespace, then tries each step a `RUN` in a user
//! namespace would need, and reports each with its errno over a pipe. The
//! two `unshare` calls are apart, as the M3 measurements made them (ADR-0099
//! evidence): on Ubuntu 24.04 the second is where a ranged map stops. The
//! child's mount namespace is owned by its user namespace, so it is less
//! privileged than this process's: the kernel turned its shared mounts into
//! slaves, the child makes `/` a recursive slave again before it mounts,
//! nothing it mounts propagates out, and all of it goes when it exits. The
//! files the steps need are made by this process in a directory the caller
//! made for the probe, and removed here.

use std::ffi::CStr;
use std::fmt;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rnk_rootfs::{OpenHow, RootPath};
use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::fs::{FileType, Mode, OFlags};
use rustix::io::Errno;
use rustix::mount::{MountFlags, MountPropagationFlags};
use rustix::pipe::PipeFlags;
use rustix::process::{DumpableBehavior, Pid, Signal, WaitOptions, WaitStatus};
use rustix::thread::UnshareFlags;
use tracing::field::Empty;

use crate::child::{self, FdPath};
use crate::error::{Error, Result, sys_err};

/// How long one attempt may take before its child is killed.
const ATTEMPT_DEADLINE: Duration = Duration::from_secs(10);

/// How long [`UsernsReport::in_namespace`]'s closure may run.
const CLOSURE_DEADLINE: Duration = Duration::from_secs(60);
/// How long a child that ended, or was killed, has to be reaped.
const REAP_GRACE: Duration = Duration::from_secs(2);

/// The most bytes of a closure's result kept.
const RESULT_LIMIT: usize = 2048;

/// Where `newuidmap` and `newgidmap` are looked for: fixed directories,
/// never `PATH`.
const TOOL_DIRS: [&str; 2] = ["/usr/bin", "/bin"];

/// Why `chown` is not tried with a map of id 0 alone.
const ONLY_ID_0: &str = "only id 0 is mapped";

/// Why a step after a failed map is not tried.
const NO_MAP: &str = "no id map";

/// Why a step is missing from a child that stopped early.
const STOPPED: &str = "the probe's child stopped before it";

// The operations of the steps that are their own label in the report.
const UNSHARE_USER: &str = "unshare(CLONE_NEWUSER)";
/// To a mapped id other than 0.
const CHOWN_OP: &str = "chown";
const UNSHARE_MOUNT: &str = "unshare(CLONE_NEWNS)";
/// Of `/dev/null`'s node, 1:3.
const MKNOD_OP: &str = "mknod";
/// In the child's mount namespace, before any mount.
const SLAVE_OP: &str = "make / a recursive slave mount";

/// Which id map the probe's user namespace got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum IdMap {
    /// Every id this process has, each mapped to itself, written by this
    /// process: root, with `CAP_SETUID` and `CAP_SETGID` (in a container,
    /// several ranges).
    Ranges,
    /// `newuidmap` and `newgidmap`: the user's own ids as 0 and its
    /// subordinate ranges from 1.
    Newuidmap,
    /// One id: this process's own uid and gid as 0, written by the
    /// namespace's process itself.
    OneId,
}

impl fmt::Display for IdMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Ranges => "multi-range",
            Self::Newuidmap => "newuidmap",
            Self::OneId => "one uid",
        })
    }
}

/// A range of ids: `count` ids from `start`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IdRange {
    /// The first id.
    pub start: u32,
    /// How many ids.
    pub count: u32,
}

/// The subordinate ids of a user other than root, for `newuidmap` and
/// `newgidmap`, as the caller read them (`/etc/subuid`, `/etc/subgid`): this
/// crate looks up no user on the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubordinateIds {
    /// The first range of `/etc/subuid` for the user.
    pub uids: IdRange,
    /// The first range of `/etc/subgid` for the user.
    pub gids: IdRange,
}

/// A step that failed: the operation, and its errno or what else is known.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepFailure {
    /// The operation refused, such as `mount proc`.
    pub op: &'static str,
    /// Its errno, when it has one.
    pub errno: Option<i32>,
    /// What else is known: a tool's exit and message.
    pub detail: Option<String>,
}

impl StepFailure {
    fn errno(op: &'static str, errno: i32) -> Self {
        Self {
            op,
            errno: Some(errno),
            detail: None,
        }
    }

    fn other(op: &'static str, detail: impl Into<String>) -> Self {
        Self {
            op,
            errno: None,
            detail: Some(detail.into()),
        }
    }

    /// The errno and what else is known, without the operation.
    fn cause(&self) -> String {
        let errno = self.errno.map(|e| match errno_name(e) {
            Some(name) => name.to_owned(),
            None => format!("errno {e}"),
        });
        [errno.as_deref(), self.detail.as_deref()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ")
    }
}

impl fmt::Display for StepFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.op, self.cause())
    }
}

/// The result of one step.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum StepResult {
    /// It worked.
    Ok,
    /// It was refused.
    Failed(StepFailure),
    /// It was not tried, for the reason given.
    NotRun(&'static str),
}

impl fmt::Display for StepResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ok => f.write_str("ok"),
            Self::Failed(e) => e.fmt(f),
            Self::NotRun(why) => write!(f, "not run ({why})"),
        }
    }
}

/// The result of a kernel filesystem's step: a new instance, else a
/// recursive bind of the one this process sees.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum MountResult {
    /// A new instance was mounted.
    Fresh,
    /// A new instance was refused; the recursive bind worked.
    Rbind {
        /// Why the new instance was refused.
        fresh: StepFailure,
    },
    /// Both were refused.
    Failed {
        /// Why the new instance was refused (or, when making `/` a slave
        /// mount was, that).
        fresh: StepFailure,
        /// Why the bind was refused; `None` when it was not tried.
        rbind: Option<StepFailure>,
    },
    /// Not tried, for the reason given.
    NotRun(&'static str),
}

impl MountResult {
    fn works(&self) -> bool {
        matches!(self, Self::Fresh | Self::Rbind { .. })
    }

    fn show(&self, f: &mut fmt::Formatter<'_>, mounted: &str) -> fmt::Result {
        match self {
            Self::Fresh => f.write_str("new instance"),
            Self::Rbind { fresh } => write!(f, "rbind of {mounted} ({fresh})"),
            Self::Failed { fresh, rbind } => {
                write!(f, "{fresh}")?;
                match rbind {
                    Some(r) => write!(f, ", then {r}"),
                    None => Ok(()),
                }
            }
            Self::NotRun(why) => write!(f, "not run ({why})"),
        }
    }
}

/// What the user-namespace probe found, step by step
/// ([`probe_user_namespace`]). The steps are those of the attempt whose map
/// worked, or of the last attempt.
#[derive(Clone, Debug)]
pub struct UsernsReport {
    /// `unshare(CLONE_NEWUSER)`.
    pub unshare: StepResult,
    /// Each map considered, in order, with its result.
    pub maps: Vec<(IdMap, StepResult)>,
    /// `chown` of a file in the probe's directory to a mapped id other
    /// than 0.
    pub chown: StepResult,
    /// `unshare(CLONE_NEWNS)`, after the map.
    pub mount_ns: StepResult,
    /// `proc`: a new instance in a new PID namespace, else an rbind of
    /// `/proc`.
    pub proc: MountResult,
    /// `sysfs`, read-only, else an rbind of `/sys`.
    pub sysfs: MountResult,
    /// `mknod` of `/dev/null`'s node, 1:3. Reported, never needed: no user
    /// namespace allows it, and the sandbox binds the nodes of `/dev`.
    pub mknod: StepResult,
    /// How the attempt's child ended when not by itself: killed at the
    /// deadline, or by a signal.
    pub cut_short: Option<String>,
    /// The map that worked, for [`UsernsReport::in_namespace`].
    plan: Option<Plan>,
}

impl UsernsReport {
    /// The map the namespace got, if any worked.
    #[must_use]
    pub fn map(&self) -> Option<IdMap> {
        self.plan.as_ref().map(Plan::kind)
    }

    /// The first step a `RUN` in a user namespace needs that failed, if
    /// any: `unshare`, `map`, `chown` (not tried with one id: an image with
    /// one owner needs none, ADR-0098), `mount namespace`, `proc` or `sysfs`
    /// (`mknod` is never needed).
    #[must_use]
    pub fn stop(&self) -> Option<&'static str> {
        if self.unshare != StepResult::Ok {
            Some("unshare")
        } else if self.map().is_none() {
            Some("map")
        } else if !matches!(self.chown, StepResult::Ok | StepResult::NotRun(ONLY_ID_0)) {
            Some("chown")
        } else if self.mount_ns != StepResult::Ok {
            Some("mount namespace")
        } else if !self.proc.works() {
            Some("proc")
        } else if !self.sysfs.works() {
            Some("sysfs")
        } else {
            None
        }
    }

    /// Runs `f(path)` as root of a new user and mount namespace with the map
    /// this probe got, in a forked child. `None` when no map worked; `Err` with
    /// why `f` did not run or finish ([`Error::NotSingleThreaded`], a
    /// host-side error, [`Error::ProbeChildLeft`], else
    /// [`Error::ProbeClosure`]); else what `f` returned. Like the steps,
    /// whatever `f` mounts goes with the child.
    ///
    /// Call while the process is single-threaded, as the set-up is: the
    /// child runs `f`, which may allocate; and before any credential is
    /// read, as for [`probe_user_namespace`]. `f` must not fork: nothing
    /// here kills or reaps a process it starts, which would outlive the
    /// child, holding its namespaces and mounts.
    pub fn in_namespace(
        &self,
        // A function, not a closure: it captures no descriptor of this
        // process, each closed in the child before it runs.
        f: fn(&Path) -> std::result::Result<String, String>,
        path: &Path,
    ) -> Option<Result<std::result::Result<String, String>>> {
        let plan = self.plan.as_ref()?;
        Some(crate::setup::require_single_thread().and_then(|()| run_closure(plan, f, path)))
    }
}

impl fmt::Display for UsernsReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // A step refused by its own operation shows only the errno.
        let step = |f: &mut fmt::Formatter<'_>, label: &str, r: &StepResult| match r {
            StepResult::Failed(e) if e.op == label => write!(f, "{label}: {}", e.cause()),
            r => write!(f, "{label}: {r}"),
        };
        step(f, UNSHARE_USER, &self.unshare)?;
        f.write_str("; map: ")?;
        match self.map() {
            Some(m) => write!(f, "{m}")?,
            None => f.write_str("none")?,
        }
        let refused: Vec<String> = self
            .maps
            .iter()
            .filter(|(_, r)| *r != StepResult::Ok)
            .map(|(m, r)| format!("{m}: {r}"))
            .collect();
        if !refused.is_empty() {
            write!(f, " ({})", refused.join(", "))?;
        }
        f.write_str("; ")?;
        step(f, CHOWN_OP, &self.chown)?;
        f.write_str("; ")?;
        step(f, UNSHARE_MOUNT, &self.mount_ns)?;
        f.write_str("; proc: ")?;
        self.proc.show(f, "/proc")?;
        f.write_str("; sysfs: ")?;
        self.sysfs.show(f, "/sys")?;
        f.write_str("; ")?;
        step(f, MKNOD_OP, &self.mknod)?;
        if let Some(c) = &self.cut_short {
            write!(f, "; {c}")?;
        }
        Ok(())
    }
}

/// Probes the user-namespace steps in `dir`, an empty directory the caller
/// made for it on the state directory's filesystem, held from where it was
/// made; what this makes there is removed before it returns. Root gets a
/// map of every id it has ([`IdMap::Ranges`]), else one id; another user
/// gets `newuidmap` with `subordinate`, else one id. Each map is one
/// attempt, in its own child, until one works or `unshare` itself is
/// refused.
///
/// Call in the single-threaded start-up window, before any credential is
/// read: each child is a copy of this process's memory. Reported only:
/// nothing here changes what the build does. One `rnk_sandbox.probe_userns`
/// span.
///
/// # Errors
///
/// [`crate::Error::NotSingleThreaded`] when other threads exist; a host-side
/// error: the probe's files, a pipe, the fork. A refused step is not an
/// error: it is in the report.
pub fn probe_user_namespace(
    dir: &RootPath,
    subordinate: Option<SubordinateIds>,
) -> Result<UsernsReport> {
    crate::setup::require_single_thread()?;
    probe_in(dir, subordinate)
}

/// What the step child works on, in the probe's directory, made and opened
/// here before the fork. The mount points, `proc` and `sys`, are opened by
/// the child: a mount needs a target in the child's own mount namespace.
struct Targets {
    /// The probe's directory (`O_PATH`): the child's working directory,
    /// which its new mount namespace carries over, and `mknodat`'s.
    dir: OwnedFd,
    /// A file the child `chown`s, opened for writing (`fchown` needs more
    /// than `O_PATH`).
    chown: OwnedFd,
}

fn probe_in(root: &RootPath, subordinate: Option<SubordinateIds>) -> Result<UsernsReport> {
    let span = tracing::debug_span!("rnk_sandbox.probe_userns", ok = Empty, map = Empty);
    let _entered = span.enter();
    let targets = make_targets(root)?;
    let euid = rustix::process::geteuid().as_raw();
    let egid = rustix::process::getegid().as_raw();
    let mut maps = Vec::new();
    let mut last = None;
    for candidate in plans(euid, egid, subordinate) {
        let plan = match candidate {
            Ok(plan) => plan,
            Err((kind, result)) => {
                maps.push((kind, result));
                continue;
            }
        };
        let attempt = attempt(&plan, &targets, euid != 0);
        let attempt = match attempt {
            Ok(a) => a,
            // A child still there may still work in the directory.
            Err(e @ Error::ProbeChildLeft { .. }) => return Err(e),
            Err(e) => {
                remove_targets(root, targets);
                return Err(e);
            }
        };
        maps.push((plan.kind(), attempt.map.clone()));
        let done = attempt.unshare != StepResult::Ok || attempt.map == StepResult::Ok;
        last = Some((plan, attempt));
        if done {
            break;
        }
    }
    remove_targets(root, targets);
    let report = report_of(maps, last);
    span.record("ok", report.stop().is_none());
    if let Some(m) = report.map() {
        span.record("map", tracing::field::display(m));
    }
    Ok(report)
}

/// The report of the attempts made: the steps of the last.
fn report_of(maps: Vec<(IdMap, StepResult)>, last: Option<(Plan, Attempt)>) -> UsernsReport {
    match last {
        Some((plan, a)) => UsernsReport {
            plan: (a.map == StepResult::Ok).then_some(plan),
            unshare: a.unshare,
            maps,
            chown: a.chown,
            mount_ns: a.mount_ns,
            proc: a.proc,
            sysfs: a.sysfs,
            mknod: a.mknod,
            cut_short: a.cut_short,
        },
        // `plans` always ends with one id; unreachable, reported as such.
        None => UsernsReport {
            unshare: StepResult::NotRun(NO_MAP),
            maps,
            chown: StepResult::NotRun(NO_MAP),
            mount_ns: StepResult::NotRun(NO_MAP),
            proc: MountResult::NotRun(NO_MAP),
            sysfs: MountResult::NotRun(NO_MAP),
            mknod: StepResult::NotRun(NO_MAP),
            cut_short: None,
            plan: None,
        },
    }
}

/// Makes the [`Targets`]; on a failure, removes what it made first.
fn make_targets(root: &RootPath) -> Result<Targets> {
    let mut made = Vec::new();
    let r = make_each(root, &mut made);
    if r.is_err() {
        for (name, dir) in made.into_iter().rev() {
            remove_one(root, name, dir);
        }
    }
    r
}

fn make_each(root: &RootPath, made: &mut Vec<(&'static str, bool)>) -> Result<Targets> {
    root.create_dir("proc", 0o700)?;
    made.push(("proc", true));
    root.create_dir("sys", 0o700)?;
    made.push(("sys", true));
    let chown = root.open_at("chown", &OpenHow::write().create_new(0o600))?;
    made.push(("chown", false));
    let dir = root.open_at(".", &OpenHow::path().directory())?;
    Ok(Targets { dir, chown })
}

/// Removes what [`make_targets`] made, and the node `mknod` may have made.
fn remove_targets(root: &RootPath, targets: Targets) {
    drop(targets);
    for (name, dir) in [
        ("mknod", false),
        ("chown", false),
        ("proc", true),
        ("sys", true),
    ] {
        remove_one(root, name, dir);
    }
}

/// Removes the probe's file or empty directory `name`; a failure is only
/// logged: the caller removes the directory too.
fn remove_one(root: &RootPath, name: &str, dir: bool) {
    let r = if dir {
        root.remove_dir(name)
    } else {
        root.remove_file(name)
    };
    if let Err(e) = r
        && !(name == "mknod" && e.raw_os_error() == Some(libc::ENOENT))
    {
        tracing::debug!("rnk-sandbox: user-namespace probe: cannot remove {name}: {e}");
    }
}

/// How one attempt maps ids.
#[derive(Clone, Debug)]
enum Plan {
    /// This process writes `uid_map` and `gid_map`, each id it has mapped
    /// to itself.
    Ranges { uid_map: Vec<u8>, gid_map: Vec<u8> },
    /// `newuidmap` and `newgidmap` write them.
    Newuidmap {
        uid: u32,
        gid: u32,
        sub: SubordinateIds,
        tools: (PathBuf, PathBuf),
    },
    /// The child writes one id of its own.
    OneId { uid_map: Vec<u8>, gid_map: Vec<u8> },
}

impl Plan {
    fn kind(&self) -> IdMap {
        match self {
            Self::Ranges { .. } => IdMap::Ranges,
            Self::Newuidmap { .. } => IdMap::Newuidmap,
            Self::OneId { .. } => IdMap::OneId,
        }
    }

    /// Whether this process writes the map (the child waits for it).
    fn by_parent(&self) -> bool {
        !matches!(self, Self::OneId { .. })
    }

    /// The smallest mapped uid and gid other than 0, inside the namespace.
    fn other_ids(&self) -> Option<(u32, u32)> {
        match self {
            Self::Ranges { uid_map, gid_map } => Some((other_id(uid_map)?, other_id(gid_map)?)),
            Self::Newuidmap { .. } => Some((1, 1)),
            Self::OneId { .. } => None,
        }
    }

    /// The map the child writes itself.
    fn own_map(&self) -> Option<(&[u8], &[u8])> {
        match self {
            Self::OneId { uid_map, gid_map } => Some((uid_map, gid_map)),
            Self::Ranges { .. } | Self::Newuidmap { .. } => None,
        }
    }
}

/// The maps to try, in order; one that cannot be tried comes with why.
fn plans(
    euid: u32,
    egid: u32,
    subordinate: Option<SubordinateIds>,
) -> Vec<std::result::Result<Plan, (IdMap, StepResult)>> {
    let mut plans = Vec::new();
    if euid == 0 {
        plans.push(ranges().map_err(|f| (IdMap::Ranges, StepResult::Failed(f))));
    } else {
        plans.push(match (subordinate, tool("newuidmap"), tool("newgidmap")) {
            (None, _, _) => Err((
                IdMap::Newuidmap,
                StepResult::NotRun("no subordinate ids for this user"),
            )),
            (Some(sub), Some(u), Some(g)) => Ok(Plan::Newuidmap {
                uid: euid,
                gid: egid,
                sub,
                tools: (u, g),
            }),
            (Some(_), _, _) => Err((
                IdMap::Newuidmap,
                StepResult::NotRun("no newuidmap and newgidmap in /usr/bin or /bin"),
            )),
        });
    }
    plans.push(Ok(Plan::OneId {
        uid_map: format!("0 {euid} 1\n").into_bytes(),
        gid_map: format!("0 {egid} 1\n").into_bytes(),
    }));
    plans
}

/// [`Plan::Ranges`] from this process's own maps.
fn ranges() -> std::result::Result<Plan, StepFailure> {
    let read = |name: &'static str| {
        std::fs::read_to_string(format!("/proc/self/{name}"))
            .map_err(|e| StepFailure::other(name, format!("cannot be read: {e}")))
            .and_then(|text| mirror(&text).ok_or_else(|| StepFailure::other(name, "unreadable")))
    };
    Ok(Plan::Ranges {
        uid_map: read("uid_map")?,
        gid_map: read("gid_map")?,
    })
}

/// The map that gives a child namespace each id of this one, as itself:
/// `inside inside count` for each `inside outside count` line of this
/// process's own map. `None` when a line is not three numbers.
fn mirror(map: &str) -> Option<Vec<u8>> {
    let mut out = String::new();
    for line in map.lines().filter(|l| !l.trim().is_empty()) {
        let mut fields = line.split_whitespace().map(str::parse::<u32>);
        let (Some(Ok(inside)), Some(Ok(_)), Some(Ok(count)), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return None;
        };
        out.push_str(&format!("{inside} {inside} {count}\n"));
    }
    (!out.is_empty()).then(|| out.into_bytes())
}

/// The smallest id other than 0 a map's text gives the namespace.
fn other_id(map: &[u8]) -> Option<u32> {
    std::str::from_utf8(map)
        .ok()?
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace().map(str::parse::<u32>);
            let (Some(Ok(inside)), Some(Ok(_)), Some(Ok(count))) = (f.next(), f.next(), f.next())
            else {
                return None;
            };
            match inside {
                0 if count > 1 => Some(1),
                0 => None,
                id => Some(id),
            }
        })
        .min()
}

/// `name` in one of [`TOOL_DIRS`], a regular file.
fn tool(name: &str) -> Option<PathBuf> {
    TOOL_DIRS
        .iter()
        .map(|d| Path::new(d).join(name))
        .find(|p| std::fs::metadata(p).is_ok_and(|m| m.is_file()))
}

/// The step results of one attempt.
struct Attempt {
    unshare: StepResult,
    map: StepResult,
    chown: StepResult,
    mount_ns: StepResult,
    proc: MountResult,
    sysfs: MountResult,
    mknod: StepResult,
    cut_short: Option<String>,
}

// The child's report: 5-byte records, an operation and its errno (0: it
// worked), in the order the child performs them.
/// `fchdir` to the probe's directory, before the namespaces.
const CHDIR: u8 = 1;
const UNSHARE: u8 = 2;
/// The child waits for this process to write the map.
const READY: u8 = 3;
const DUMPABLE: u8 = 4;
const UID_MAP: u8 = 5;
const SETGROUPS: u8 = 6;
const GID_MAP: u8 = 7;
const CHOWN: u8 = 8;
const NEWNS: u8 = 9;
const SLAVE: u8 = 10;
/// Opening the mount points in the child's mount namespace.
const TARGETS: u8 = 11;
const NEWPID: u8 = 12;
const PID1: u8 = 13;
const PROC: u8 = 14;
const PROC_RBIND: u8 = 15;
const SYSFS: u8 = 16;
const SYSFS_RBIND: u8 = 17;
const MKNOD: u8 = 18;
const DONE: u8 = 19;
/// [`run_closure`]'s result: the errno field holds the outcome (bits 16 to
/// 23) and the length of the text that follows (bits 0 to 15).
const RESULT: u8 = 20;

/// What a child needs, prepared before the fork: it only reads these.
struct ChildArgs<'a> {
    parent: Pid,
    report: RawFd,
    go: RawFd,
    /// A user other than root: the child is made dumpable around the map,
    /// whose write needs its `/proc` files to be the user's.
    user: bool,
    /// The map the child writes itself (one id), else `None`: this process
    /// writes it.
    own_map: Option<(&'a [u8], &'a [u8])>,
    /// The `chown` target, and the ids to give it.
    chown: Option<(RawFd, u32, u32)>,
    /// The probe's directory; -1 for none.
    dir: RawFd,
}

/// One attempt with `plan`: the child steps, reported.
fn attempt(plan: &Plan, t: &Targets, user: bool) -> Result<Attempt> {
    let chown = plan.other_ids().map(|(u, g)| (t.chown.as_raw_fd(), u, g));
    let run = supervise(
        plan,
        user,
        chown,
        t.dir.as_raw_fd(),
        ATTEMPT_DEADLINE,
        |args| {
            // SAFETY: the child of the fork, right after it returned 0.
            unsafe { step_child(args) }
        },
    )?;
    Ok(assemble(
        plan,
        &run.records.list,
        run.parent_map,
        run.cut_short,
    ))
}

/// What [`supervise`] saw of a child.
struct Supervised {
    records: Records,
    /// The map this process wrote, when the child asked for it.
    parent_map: Option<std::result::Result<(), StepFailure>>,
    cut_short: Option<String>,
}

/// Forks a child that runs `child`, writes its map when it asks
/// ([`READY`]), reads its report until it closes it or `deadline` passes,
/// kills it then, and reaps it ([`reap`]); [`Error::ProbeChildLeft`] when
/// it is still there after that.
///
/// `child` runs in the forked process, which ends with `_exit` when it
/// returns; it may allocate only when this process is single-threaded. It
/// must use and drop no owner of a descriptor of this process: every one
/// but stdio and those in [`ChildArgs`] is closed before it runs.
fn supervise(
    plan: &Plan,
    user: bool,
    chown: Option<(RawFd, u32, u32)>,
    dir: RawFd,
    deadline: Duration,
    child: impl FnOnce(&ChildArgs<'_>),
) -> Result<Supervised> {
    let (report_r, report_w) = rustix::pipe::pipe_with(PipeFlags::CLOEXEC)
        .map_err(|e| sys_err("make the probe's pipe", e))?;
    let (go_r, go_w) = rustix::pipe::pipe_with(PipeFlags::CLOEXEC)
        .map_err(|e| sys_err("make the probe's pipe", e))?;
    let args = ChildArgs {
        parent: rustix::process::getpid(),
        report: report_w.as_raw_fd(),
        go: go_r.as_raw_fd(),
        user,
        own_map: plan.own_map(),
        chown,
        dir,
    };
    // SAFETY: the child runs `child`, then `_exit`; what it may do between
    // is the caller's contract above.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        // This process's ends first, whatever `close_unused` manages: with
        // its own copy of the go pipe's write end, a child would never read
        // the end of a map this process failed to write.
        drop(go_w);
        drop(report_r);
        // SAFETY: the child of the fork, which ends with `_exit`; `child`
        // uses and drops no owner of a descriptor this closes (the contract
        // above).
        unsafe { close_unused(&args) };
        child(&args);
        // SAFETY: `_exit` ends this process at once, after `child` if it
        // returns.
        unsafe { libc::_exit(0) }
    }
    let fork_error = io::Error::last_os_error();
    drop(report_w);
    drop(go_r);
    let pid = (pid > 0)
        .then(|| Pid::from_raw(pid))
        .flatten()
        .ok_or_else(|| sys_err("fork the probe's child", fork_error))?;
    let mut go = Some(go_w);
    let mut parent_map = None;
    let read = read_records(&report_r, Instant::now() + deadline, |op| {
        if op == READY
            && let Some(go) = go.take()
        {
            let r = write_map(plan, pid);
            if r.is_ok() {
                // A child gone reads nothing; its report says so.
                let _ = rustix::io::write(&go, b"g");
            }
            parent_map = Some(r);
        }
    });
    // Closed without a byte, the child stops instead of going on unmapped.
    drop(go);
    let killed = read.as_ref().map_or(true, |r| r.timed_out);
    if killed {
        // `ESRCH` when it already exited.
        let _ = rustix::process::kill_process(pid, Signal::KILL);
    }
    let reaped = reap(pid, killed, REAP_GRACE);
    if let Ok(None) = reaped {
        return Err(Error::ProbeChildLeft {
            pid: pid.as_raw_nonzero().get(),
        });
    }
    let records = read?;
    let status = reaped.map_err(|e| sys_err("reap the probe's child", e))?;
    let cut_short = if records.timed_out {
        Some(format!(
            "the probe's child was killed after {} s",
            deadline.as_secs()
        ))
    } else {
        status
            .and_then(|st| st.terminating_signal())
            .map(|sig| format!("the probe's child died of signal {sig}"))
    };
    Ok(Supervised {
        records,
        parent_map,
        cut_short,
    })
}

/// Reaps `pid`, polling for `grace`: one not `killed` yet that is still
/// there then is killed, and polled for as long again. `None` when it is
/// still there after, in the kernel (`SIGKILL` cannot be caught), where it
/// may still finish a call that writes.
fn reap(pid: Pid, mut killed: bool, grace: Duration) -> rustix::io::Result<Option<WaitStatus>> {
    let mut deadline = Instant::now() + grace;
    loop {
        match rustix::process::waitpid(Some(pid), WaitOptions::NOHANG) {
            Ok(Some((_, st))) => return Ok(Some(st)),
            Ok(None) | Err(Errno::INTR) => {}
            Err(e) => return Err(e),
        }
        if Instant::now() >= deadline {
            if killed {
                return Ok(None);
            }
            // `ESRCH` when it exited meanwhile; the next poll reaps it.
            let _ = rustix::process::kill_process(pid, Signal::KILL);
            killed = true;
            deadline = Instant::now() + grace;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Closes, right after the fork, every descriptor the child copied from
/// this process but those `a` names and stdio, which a later `open` must
/// not take: ranikust's others (the state directory, the build context, a
/// registry connection) are not the child's to use.
///
/// # Safety
///
/// As [`child::close_all_except`]: only in a forked child that ends with
/// `_exit` and uses or drops no owner of a descriptor this closes.
unsafe fn close_unused(a: &ChildArgs<'_>) {
    let mut keep = [0, 1, 2, a.report, a.go, a.dir, a.chown.map_or(-1, |c| c.0)];
    keep.sort_unstable();
    // SAFETY: the caller's contract, which is `close_all_except`'s.
    unsafe { child::close_all_except(&keep) };
}

/// Writes the child's map from here: [`Plan::Ranges`] directly, as root
/// may; [`Plan::Newuidmap`] through the setuid tools.
fn write_map(plan: &Plan, pid: Pid) -> std::result::Result<(), StepFailure> {
    match plan {
        Plan::Ranges { uid_map, gid_map } => {
            write_proc(pid, "uid_map", "write uid_map", uid_map)?;
            write_proc(pid, "gid_map", "write gid_map", gid_map)
        }
        Plan::Newuidmap {
            uid,
            gid,
            sub,
            tools,
        } => {
            run_tool("newuidmap", &tools.0, pid, *uid, sub.uids)?;
            run_tool("newgidmap", &tools.1, pid, *gid, sub.gids)
        }
        Plan::OneId { .. } => Ok(()),
    }
}

/// Writes `text` to `/proc/<pid>/<name>` in one write, as the kernel asks;
/// a failure is reported as `op`'s.
fn write_proc(
    pid: Pid,
    name: &str,
    op: &'static str,
    text: &[u8],
) -> std::result::Result<(), StepFailure> {
    let path = format!("/proc/{}/{name}", pid.as_raw_nonzero());
    let fd = rustix::fs::open(
        path.as_str(),
        OFlags::WRONLY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| StepFailure::errno(op, e.raw_os_error()))?;
    match rustix::io::write(&fd, text) {
        Ok(n) if n == text.len() => Ok(()),
        Ok(_) => Err(StepFailure::other(op, "short write")),
        Err(e) => Err(StepFailure::errno(op, e.raw_os_error())),
    }
}

/// Runs `newuidmap` or `newgidmap` for `pid`: the own id as 0, the range
/// from 1. No environment, no input; the first line of its error output is
/// kept.
fn run_tool(
    op: &'static str,
    path: &Path,
    pid: Pid,
    own: u32,
    range: IdRange,
) -> std::result::Result<(), StepFailure> {
    let mut child = std::process::Command::new(path)
        .args([
            pid.as_raw_nonzero().to_string(),
            "0".to_owned(),
            own.to_string(),
            "1".to_owned(),
            "1".to_owned(),
            range.start.to_string(),
            range.count.to_string(),
        ])
        .env_clear()
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| StepFailure::other(op, format!("cannot be run: {e}")))?;
    let deadline = Instant::now() + ATTEMPT_DEADLINE;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            Ok(None) => {
                end_tool(&mut child);
                return Err(StepFailure::other(op, "did not finish"));
            }
            Err(e) => {
                if kill_after_wait_error(&e) {
                    end_tool(&mut child);
                }
                return Err(StepFailure::other(op, format!("cannot be waited for: {e}")));
            }
        }
    };
    if status.success() {
        return Ok(());
    }
    let mut bytes = Vec::new();
    if let Some(mut err) = child.stderr.take() {
        use std::io::Read as _;
        // What was read before an error is kept; the exit status is enough.
        let _ = (&mut err).take(512).read_to_end(&mut bytes);
    }
    let text = String::from_utf8_lossy(&bytes);
    let line: String = text
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(200)
        .collect();
    Err(StepFailure::other(op, format!("{status}: {line}")))
}

/// Whether a tool whose wait failed with `e` is killed: not after `ECHILD`,
/// as it was reaped elsewhere and its number may be another process's by
/// now.
fn kill_after_wait_error(e: &io::Error) -> bool {
    e.raw_os_error() != Some(Errno::CHILD.raw_os_error())
}

/// Kills a tool that did not end, and reaps it within [`REAP_GRACE`]; one
/// still there after is left, as it holds nothing of the probe's.
fn end_tool(child: &mut std::process::Child) {
    // A tool that exited meanwhile makes `kill` fail; the attempt fails
    // either way.
    let _ = child.kill();
    let deadline = Instant::now() + REAP_GRACE;
    while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The records read, and whether the deadline came first.
struct Records {
    list: Vec<(u8, i32)>,
    payload: Option<(u8, Vec<u8>)>,
    timed_out: bool,
}

/// Reads the child's report until it closes it or `deadline`, calling
/// `on_record` with each operation as it comes (the map is written on
/// [`READY`], while the child waits).
fn read_records(fd: &OwnedFd, deadline: Instant, mut on_record: impl FnMut(u8)) -> Result<Records> {
    let mut out = Records {
        list: Vec::new(),
        payload: None,
        timed_out: false,
    };
    let mut buf = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            out.timed_out = true;
            return Ok(out);
        }
        let timeout = Timespec {
            tv_sec: i64::try_from(left.as_secs()).unwrap_or(i64::MAX),
            tv_nsec: i64::from(left.subsec_nanos()),
        };
        let mut fds = [PollFd::new(fd, PollFlags::IN)];
        match rustix::event::poll(&mut fds, Some(&timeout)) {
            Ok(0) | Err(Errno::INTR) => continue,
            Ok(_) => {}
            Err(e) => return Err(sys_err("poll the probe's report", e)),
        }
        let mut chunk = [0u8; 4096];
        let n = match rustix::io::read(fd, &mut chunk) {
            Ok(0) => return Ok(out),
            Ok(n) => n,
            Err(Errno::INTR | Errno::AGAIN) => continue,
            Err(e) => return Err(sys_err("read the probe's report", e)),
        };
        buf.extend_from_slice(&chunk[..n]);
        while let Some(&[op, a, b, c, d]) = buf.get(..5) {
            let value = i32::from_le_bytes([a, b, c, d]);
            if op == RESULT {
                let len = usize::try_from(value & 0xffff).unwrap_or(0);
                let Some(text) = buf.get(5..5 + len) else {
                    break;
                };
                let status = u8::try_from((value >> 16) & 0xff).unwrap_or(u8::MAX);
                out.payload = Some((status, text.to_vec()));
                buf.drain(..5 + len);
                continue;
            }
            buf.drain(..5);
            out.list.push((op, value));
            on_record(op);
        }
    }
}

/// The step results from the child's records and the map this process
/// wrote, if it did.
fn assemble(
    plan: &Plan,
    records: &[(u8, i32)],
    parent_map: Option<std::result::Result<(), StepFailure>>,
    cut_short: Option<String>,
) -> Attempt {
    let get = |op: u8| records.iter().find(|r| r.0 == op).map(|r| r.1);
    let step = |op: u8, name: &'static str| match get(op) {
        Some(0) => StepResult::Ok,
        Some(e) => StepResult::Failed(StepFailure::errno(name, e)),
        None => StepResult::NotRun(STOPPED),
    };
    let unshare = match get(CHDIR).filter(|&e| e != 0) {
        Some(e) => StepResult::Failed(StepFailure::errno("fchdir to the probe's directory", e)),
        None => step(UNSHARE, UNSHARE_USER),
    };
    let map = if let Some(e) = get(DUMPABLE).filter(|&e| e != 0) {
        StepResult::Failed(StepFailure::errno("prctl(PR_SET_DUMPABLE)", e))
    } else if unshare != StepResult::Ok {
        StepResult::NotRun("no user namespace")
    } else if plan.by_parent() {
        match parent_map {
            Some(Ok(())) => StepResult::Ok,
            Some(Err(f)) => StepResult::Failed(f),
            None => StepResult::NotRun(STOPPED),
        }
    } else {
        [
            step(UID_MAP, "write uid_map"),
            step(SETGROUPS, "write setgroups"),
            step(GID_MAP, "write gid_map"),
        ]
        .into_iter()
        .find(|r| *r != StepResult::Ok)
        .unwrap_or(StepResult::Ok)
    };
    if map != StepResult::Ok {
        return Attempt {
            unshare,
            map,
            chown: StepResult::NotRun(NO_MAP),
            mount_ns: StepResult::NotRun(NO_MAP),
            proc: MountResult::NotRun(NO_MAP),
            sysfs: MountResult::NotRun(NO_MAP),
            mknod: StepResult::NotRun(NO_MAP),
            cut_short,
        };
    }
    let chown = if plan.other_ids().is_none() {
        StepResult::NotRun(ONLY_ID_0)
    } else {
        step(CHOWN, CHOWN_OP)
    };
    let mount_ns = step(NEWNS, UNSHARE_MOUNT);
    let fs = |fresh: &[(u8, &'static str)], rbind: (u8, &'static str)| {
        if mount_ns != StepResult::Ok {
            return MountResult::NotRun("no mount namespace");
        }
        let before = [(SLAVE, SLAVE_OP), (TARGETS, "open the mount points")];
        if let Some(fresh) = before.iter().find_map(|&(op, name)| {
            get(op)
                .filter(|&e| e != 0)
                .map(|e| StepFailure::errno(name, e))
        }) {
            return MountResult::Failed { fresh, rbind: None };
        }
        let failed = fresh.iter().find_map(|&(op, name)| match get(op) {
            Some(0) => None,
            Some(e) => Some(StepFailure::errno(name, e)),
            None => Some(StepFailure::other(name, "not reported")),
        });
        let Some(fresh) = failed else {
            return MountResult::Fresh;
        };
        match get(rbind.0) {
            Some(0) => MountResult::Rbind { fresh },
            Some(e) => MountResult::Failed {
                fresh,
                rbind: Some(StepFailure::errno(rbind.1, e)),
            },
            None if fresh.errno.is_none() => MountResult::NotRun(STOPPED),
            None => MountResult::Failed { fresh, rbind: None },
        }
    };
    let proc = fs(
        &[
            (NEWPID, "unshare(CLONE_NEWPID)"),
            (PID1, "fork into the PID namespace"),
            (PROC, "mount proc"),
        ],
        (PROC_RBIND, "rbind /proc"),
    );
    let sysfs = fs(&[(SYSFS, "mount sysfs")], (SYSFS_RBIND, "rbind /sys"));
    Attempt {
        unshare,
        map,
        chown,
        mount_ns,
        proc,
        sysfs,
        mknod: step(MKNOD, MKNOD_OP),
        cut_short,
    }
}

/// Sends one record. With the parent gone, the write's `SIGPIPE` ends the
/// child, which nothing waits for any more.
fn send(report: BorrowedFd<'_>, op: u8, r: rustix::io::Result<()>) {
    let e = match r {
        Ok(()) => 0,
        Err(e) => e.raw_os_error(),
    };
    let [a, b, c, d] = e.to_le_bytes();
    let _ = rustix::io::write(report, &[op, a, b, c, d]);
}

/// The child's common start: default signals, death with this process,
/// the probe's directory as working directory, the user namespace, the
/// map. `false` when the child must stop (reported).
fn enter(a: &ChildArgs<'_>, report: BorrowedFd<'_>) -> bool {
    child::default_signals();
    let armed = rustix::process::set_parent_process_death_signal(Some(Signal::KILL)).is_ok();
    if !armed || rustix::process::getppid() != Some(a.parent) {
        return false;
    }
    if a.dir >= 0 {
        // SAFETY: the probe's directory, open in this process.
        let dir = unsafe { BorrowedFd::borrow_raw(a.dir) };
        // The mount namespace made later moves the working directory to its
        // own copy of the mount, so relative paths resolve in it.
        let r = rustix::process::fchdir(dir);
        send(report, CHDIR, r);
        if r.is_err() {
            return false;
        }
    }
    // SAFETY: `NEWUSER` does not unshare the descriptor table, the only
    // flag `unshare_unsafe` is unsafe for; this child is single-threaded.
    let r = unsafe { rustix::thread::unshare_unsafe(UnshareFlags::NEWUSER) };
    send(report, UNSHARE, r);
    r.is_ok() && map_in_child(a, report)
}

/// The mount namespace, owned by the new user namespace, with `/` a
/// recursive slave. `false` when either is refused (reported).
fn enter_mount_namespace(report: BorrowedFd<'_>) -> bool {
    // SAFETY: `NEWNS` does not unshare the descriptor table; this child is
    // single-threaded.
    let r = unsafe { rustix::thread::unshare_unsafe(UnshareFlags::NEWNS) };
    send(report, NEWNS, r);
    if r.is_err() {
        return false;
    }
    let slave = rustix::mount::mount_change(
        c"/",
        MountPropagationFlags::DOWNSTREAM | MountPropagationFlags::REC,
    );
    send(report, SLAVE, slave);
    slave.is_ok()
}

/// Gets the map: waits for this process's write, or writes one id. A user
/// other than root is made dumpable for it, then not dumpable again.
fn map_in_child(a: &ChildArgs<'_>, report: BorrowedFd<'_>) -> bool {
    if a.user
        && let Err(e) = rustix::process::set_dumpable_behavior(DumpableBehavior::Dumpable)
    {
        send(report, DUMPABLE, Err(e));
        return false;
    }
    let mapped = match a.own_map {
        None => {
            send(report, READY, Ok(()));
            // SAFETY: the go pipe's read end, open in this process.
            let go = unsafe { BorrowedFd::borrow_raw(a.go) };
            let mut byte = [0u8];
            loop {
                match rustix::io::read(go, &mut byte) {
                    Err(Errno::INTR) => {}
                    r => break matches!(r, Ok(1)) && byte[0] == b'g',
                }
            }
        }
        Some((uid_map, gid_map)) => {
            let write = |op, path: &CStr, text: &[u8]| {
                let r = rustix::fs::open(path, OFlags::WRONLY | OFlags::CLOEXEC, Mode::empty())
                    .and_then(|fd| rustix::io::write(&fd, text))
                    .and_then(|n| {
                        if n == text.len() {
                            Ok(())
                        } else {
                            Err(Errno::INVAL)
                        }
                    });
                send(report, op, r);
                r.is_ok()
            };
            write(UID_MAP, c"/proc/self/uid_map", uid_map)
                && write(SETGROUPS, c"/proc/self/setgroups", b"deny")
                && write(GID_MAP, c"/proc/self/gid_map", gid_map)
        }
    };
    if a.user
        && let Err(e) = rustix::process::set_dumpable_behavior(DumpableBehavior::NotDumpable)
    {
        send(report, DUMPABLE, Err(e));
        return false;
    }
    mapped
}

/// Opens the mount point `name` in the working directory, without following
/// a symlink: in this child's mount namespace, unlike any descriptor opened
/// before it.
fn mount_point(name: &CStr) -> rustix::io::Result<(OwnedFd, FdPath)> {
    let fd = rustix::fs::openat(
        rustix::fs::CWD,
        name,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let path = FdPath::new(fd.as_raw_fd());
    Ok((fd, path))
}

/// The step child: the user namespace and the map, then each step,
/// reported in order. Only system calls on what `a` holds.
///
/// # Safety
///
/// Call only in the child of a fork, right after it returned 0.
unsafe fn step_child(a: &ChildArgs<'_>) -> ! {
    // SAFETY: the report pipe's write end, open in this process.
    let report = unsafe { BorrowedFd::borrow_raw(a.report) };
    if enter(a, report) {
        if let Some((fd, uid, gid)) = a.chown {
            // SAFETY: the target file's descriptor, open in this process.
            let file = unsafe { BorrowedFd::borrow_raw(fd) };
            let r = rustix::fs::fchown(
                file,
                Some(rustix::fs::Uid::from_raw(uid)),
                Some(rustix::fs::Gid::from_raw(gid)),
            );
            send(report, CHOWN, r);
        }
        if enter_mount_namespace(report) {
            mount_steps(report);
        }
        // SAFETY: the probe's directory, open in this process.
        let dir = unsafe { BorrowedFd::borrow_raw(a.dir) };
        let node = rustix::fs::mknodat(
            dir,
            c"mknod",
            FileType::CharacterDevice,
            Mode::from_raw_mode(0o600),
            rustix::fs::makedev(1, 3),
        );
        send(report, MKNOD, node);
        send(report, DONE, Ok(()));
    }
    // SAFETY: `_exit` ends this process at once.
    unsafe { libc::_exit(0) }
}

/// `proc` and `sysfs`, each new else an rbind, on the probe's mount points.
fn mount_steps(report: BorrowedFd<'_>) {
    let targets = mount_point(c"proc").and_then(|p| Ok((p, mount_point(c"sys")?)));
    let paths = match &targets {
        Ok(((_, proc), (_, sys))) => proc.as_cstr().zip(sys.as_cstr()).ok_or(Errno::NAMETOOLONG),
        Err(e) => Err(*e),
    };
    send(report, TARGETS, paths.map(|_| ()));
    let Ok((proc, sys)) = paths else {
        return;
    };
    if !fresh_proc(proc, report) {
        let r = rustix::mount::mount_bind_recursive(c"/proc", proc);
        send(report, PROC_RBIND, r);
    }
    let flags = MountFlags::RDONLY | MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC;
    let r = rustix::mount::mount(c"sysfs", sys, c"sysfs", flags, None);
    send(report, SYSFS, r);
    if r.is_err() {
        let r = rustix::mount::mount_bind_recursive(c"/sys", sys);
        send(report, SYSFS_RBIND, r);
    }
}

/// A new `proc` on `target`: needs a PID namespace this user namespace
/// owns, so a new one and a child in it, which mounts and reports [`PROC`]
/// itself. `true` when it mounted.
fn fresh_proc(target: &CStr, report: BorrowedFd<'_>) -> bool {
    // SAFETY: `NEWPID` does not unshare the descriptor table; this child is
    // single-threaded.
    let r = unsafe { rustix::thread::unshare_unsafe(UnshareFlags::NEWPID) };
    send(report, NEWPID, r);
    if r.is_err() {
        return false;
    }
    // SAFETY: this child is single-threaded; its child makes only system
    // calls and ends with `_exit`.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        send(report, PID1, Err(last_errno()));
        return false;
    }
    if pid == 0 {
        // PID 1 of the new namespace dies with the step child; the
        // namespace, and the mount with it, go with PID 1.
        let _ = rustix::process::set_parent_process_death_signal(Some(Signal::KILL));
        let flags = MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC;
        let r = rustix::mount::mount(c"proc", target, c"proc", flags, None);
        send(report, PROC, r);
        // SAFETY: `_exit` ends this process at once.
        unsafe { libc::_exit(i32::from(r.is_err())) }
    }
    send(report, PID1, Ok(()));
    let mut status: libc::c_int = 0;
    loop {
        // SAFETY: `waitpid` writes the status, which lives across the call.
        let r = unsafe { libc::waitpid(pid, &raw mut status, 0) };
        if r == pid {
            break;
        }
        if r < 0 && last_errno() != Errno::INTR {
            return false;
        }
    }
    libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0
}

/// This thread's `errno`.
fn last_errno() -> Errno {
    Errno::from_raw_os_error(io::Error::last_os_error().raw_os_error().unwrap_or(0))
}

/// [`UsernsReport::in_namespace`]: `f(path)` in a child with `plan`'s map.
/// The child runs `f`, which may allocate: call while single-threaded
/// (tests rely on the C library's allocator, safe after a fork).
fn run_closure(
    plan: &Plan,
    f: fn(&Path) -> std::result::Result<String, String>,
    path: &Path,
) -> Result<std::result::Result<String, String>> {
    let user = !rustix::process::geteuid().is_root();
    let run = supervise(plan, user, None, -1, CLOSURE_DEADLINE, |args| {
        // SAFETY: the child of the fork, right after it returned 0, of a
        // single-threaded process (the caller's contract): it may run `f`.
        unsafe { closure_child(args, || f(path)) }
    })?;
    if let Some((status, text)) = run.records.payload {
        return closure_outcome(status, String::from_utf8_lossy(&text).into_owned());
    }
    // No result: the child died, or a step before `f` was refused this
    // time.
    if let Some(c) = run.cut_short {
        return Err(Error::ProbeClosure(c));
    }
    let a = assemble(plan, &run.records.list, run.parent_map, None);
    let slave = run
        .records
        .list
        .iter()
        .find(|&&(op, e)| op == SLAVE && e != 0)
        .map(|&(_, e)| StepFailure::errno(SLAVE_OP, e));
    let refused = [a.unshare, a.map, a.mount_ns]
        .into_iter()
        .find_map(|r| match r {
            StepResult::Failed(f) => Some(f),
            StepResult::Ok | StepResult::NotRun(_) => None,
        })
        .or(slave);
    Err(Error::ProbeClosure(refused.map_or_else(
        || "the probe's child stopped before it ran".to_owned(),
        |f| f.to_string(),
    )))
}

/// The closure's child: the user namespace and the map, the mount
/// namespace, then `f`, whose result goes back as a [`RESULT`] record.
///
/// # Safety
///
/// Call only in the child of a fork of a single-threaded process, right
/// after it returned 0.
unsafe fn closure_child(
    a: &ChildArgs<'_>,
    f: impl FnOnce() -> std::result::Result<String, String>,
) -> ! {
    // SAFETY: the report pipe's write end, open in this process.
    let report = unsafe { BorrowedFd::borrow_raw(a.report) };
    if enter(a, report) && enter_mount_namespace(report) {
        finish_closure(report, f);
    }
    // SAFETY: `_exit` ends this process at once, running no destructor.
    unsafe { libc::_exit(0) }
}

/// Runs `f` silenced ([`run_silenced`]) and sends its [`RESULT`] record.
fn finish_closure(report: BorrowedFd<'_>, f: impl FnOnce() -> std::result::Result<String, String>) {
    let (status, text) = run_silenced(f);
    // A parent gone reads nothing; one still here sees no result.
    let _ = write_all(report, &result_frame(status, &text));
}

/// Runs `f` in a probe's child without the panic hook it inherited, and,
/// on this thread, without the `tracing` subscriber: they write ranikust's
/// metrics report and the build's log, which are the parent's to write (a
/// thread `f` starts gets the global one unless it carries this thread's
/// dispatcher over, as `rnk-snapshot`'s mount thread does). Its [`RESULT`]
/// status and text; a panic is reported there only.
fn run_silenced(f: impl FnOnce() -> std::result::Result<String, String>) -> (u8, String) {
    // Leaked, not dropped: dropping the parent's hook would run its
    // destructors in this copy of the parent.
    std::mem::forget(std::panic::take_hook());
    std::panic::set_hook(Box::new(|_| {}));
    let none = tracing::Dispatch::none();
    let ran = tracing::dispatcher::with_default(&none, || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
    });
    match ran {
        Ok(Ok(t)) => (CLOSURE_OK, t),
        Ok(Err(t)) => (CLOSURE_REFUSED, t),
        Err(_) => (CLOSURE_PANICKED, "panicked".to_owned()),
    }
}

/// The closure's [`RESULT`] statuses, from [`closure_child`] to
/// [`closure_outcome`]: it returned `Ok`, it returned `Err`, it panicked.
const CLOSURE_OK: u8 = 0;
const CLOSURE_REFUSED: u8 = 1;
const CLOSURE_PANICKED: u8 = 2;

/// What [`UsernsReport::in_namespace`] returns for the closure's status and
/// text.
fn closure_outcome(status: u8, text: String) -> Result<std::result::Result<String, String>> {
    match status {
        CLOSURE_OK => Ok(Ok(text)),
        CLOSURE_REFUSED => Ok(Err(text)),
        // A panic did not finish `f`, as under `panic = "abort"`.
        CLOSURE_PANICKED => Err(Error::ProbeClosure(text)),
        other => Err(Error::ProbeClosure(format!(
            "unknown closure status {other}: {text}"
        ))),
    }
}

/// A [`RESULT`] record: `status`, and `text` cut to [`RESULT_LIMIT`] bytes
/// on a character boundary.
fn result_frame(status: u8, text: &str) -> Vec<u8> {
    let mut end = text.len().min(RESULT_LIMIT);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let len = i32::try_from(end).unwrap_or(0);
    let [a, b, c, d] = (len | (i32::from(status) << 16)).to_le_bytes();
    let mut frame = vec![RESULT, a, b, c, d];
    frame.extend_from_slice(&text.as_bytes()[..end]);
    frame
}

fn write_all(fd: BorrowedFd<'_>, mut data: &[u8]) -> rustix::io::Result<()> {
    while !data.is_empty() {
        match rustix::io::write(fd, data) {
            Ok(n) => data = &data[n..],
            Err(Errno::INTR) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// The symbolic name of the errnos the steps may meet.
fn errno_name(e: i32) -> Option<&'static str> {
    Some(match e {
        libc::EPERM => "EPERM",
        libc::ENOENT => "ENOENT",
        libc::ESRCH => "ESRCH",
        libc::EINTR => "EINTR",
        libc::EIO => "EIO",
        libc::EBADF => "EBADF",
        libc::EAGAIN => "EAGAIN",
        libc::ENOMEM => "ENOMEM",
        libc::EACCES => "EACCES",
        libc::EFAULT => "EFAULT",
        libc::EBUSY => "EBUSY",
        libc::EEXIST => "EEXIST",
        libc::EXDEV => "EXDEV",
        libc::ENODEV => "ENODEV",
        libc::ENOTDIR => "ENOTDIR",
        libc::EINVAL => "EINVAL",
        libc::ENFILE => "ENFILE",
        libc::EMFILE => "EMFILE",
        libc::ENOSPC => "ENOSPC",
        libc::EROFS => "EROFS",
        libc::ENOSYS => "ENOSYS",
        libc::ELOOP => "ELOOP",
        libc::EUSERS => "EUSERS",
        libc::EOPNOTSUPP => "EOPNOTSUPP",
        libc::EOVERFLOW => "EOVERFLOW",
        libc::ETIMEDOUT => "ETIMEDOUT",
        _ => return None,
    })
}

#[cfg(test)]
mod tests;
